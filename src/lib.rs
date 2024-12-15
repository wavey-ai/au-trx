use gen_id::{ConfigPreset::ShortEpochMaxNodes, IdGenerator, DEFAULT_EPOCH};
use rtrb::{Consumer, Producer, RingBuffer};
use soundkit::{
    audio_packet::FrameHeader,
    audio_types::{EncodingFlag, Endianness},
};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing::error;

const BITS_PER_SAMPLE: u8 = 24;

pub struct AudioProcessor {
    producer: Option<Producer<Vec<u8>>>,
    consumer: Option<Consumer<Vec<u8>>>,
    data_available: Arc<(Mutex<bool>, Condvar)>,
    config_ready: Option<Arc<(Mutex<(Option<usize>, Option<usize>)>, Condvar)>>,
    shutdown: Arc<AtomicBool>,
    tx_thread: Option<JoinHandle<()>>,
}

impl AudioProcessor {
    pub fn new(num_channels: usize) -> Self {
        let (producer, consumer) = RingBuffer::new(1024 * 5 * 2 * 3); // 24-bit requires 3 bytes per sample
        let data_available = Arc::new((Mutex::new(false), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut proc = AudioProcessor {
            producer: Some(producer),
            consumer: Some(consumer),
            data_available,
            config_ready: None,
            shutdown,
            tx_thread: None,
        };

        proc.start_tx(num_channels);
        proc
    }

    fn start_tx(&mut self, _num_channels: usize) {
        let mut consumer = self.consumer.take().unwrap();
        let data_available = Arc::clone(&self.data_available);
        let shutdown = Arc::clone(&self.shutdown);

        let config_ready = Arc::new((Mutex::new((None::<usize>, None::<usize>)), Condvar::new()));
        let config_ready_clone = Arc::clone(&config_ready);
        self.config_ready = Some(config_ready);

        let handle = thread::spawn(move || {
            let tcp_address = "127.0.0.1:4242";
            let gen = IdGenerator::new(ShortEpochMaxNodes, DEFAULT_EPOCH);

            let (lock, cvar) = &*config_ready_clone;
            let mut config = lock.lock().unwrap();
            while config.0.is_none() || config.1.is_none() {
                if shutdown.load(Ordering::SeqCst) {
                    return;
                }
                let result = cvar
                    .wait_timeout(config, Duration::from_millis(50))
                    .unwrap();
                config = result.0;
            }

            let (samples_per_channel, num_channels) = (config.0.unwrap(), config.1.unwrap());
            let pcm_capacity = samples_per_channel * num_channels * 3; // 24-bit requires 3 bytes per sample
            drop(config);

            while !shutdown.load(Ordering::SeqCst) {
                let mut stream = match TcpStream::connect(tcp_address) {
                    Ok(stream) => stream,
                    Err(_) => {
                        thread::sleep(Duration::from_millis(50));
                        continue;
                    }
                };

                if shutdown.load(Ordering::SeqCst) {
                    break;
                }

                stream.write_all(b"HELO").ok();

                let mut id_buffer = [0u8; 2];
                if stream.read_exact(&mut id_buffer).is_err() {
                    continue;
                }

                let received_id = u16::from_le_bytes(id_buffer);
                let id = gen.next_id(received_id);

                let header = FrameHeader::new(
                    EncodingFlag::PCMSigned,
                    samples_per_channel as u16,
                    48000,
                    num_channels as u8,
                    BITS_PER_SAMPLE,
                    Endianness::LittleEndian,
                    Some(id),
                )
                .expect("header encoding failed");

                let mut header_data = Vec::with_capacity(header.size());
                header.encode(&mut header_data).ok();

                let buf_capacity = pcm_capacity + header_data.len() + 4;

                let mut send_buffer = Vec::with_capacity(buf_capacity);

                let mut d = false;
                loop {
                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    send_buffer.clear();
                    send_buffer.extend((buf_capacity as u32).to_le_bytes());
                    send_buffer.extend_from_slice(&header_data);

                    if let Err(e) = FrameHeader::validate_header(&header_data) {
                        error!("Invalid header: {}", e);
                    }

                    let (lock, cvar) = &*data_available;
                    let mut available = lock.lock().unwrap();
                    while !*available {
                        if shutdown.load(Ordering::SeqCst) {
                            break;
                        }
                        let result = cvar
                            .wait_timeout(available, Duration::from_millis(50))
                            .unwrap();
                        available = result.0;
                    }
                    *available = false;

                    while let Ok(data) = consumer.pop() {
                        send_buffer.extend_from_slice(&data);
                        if send_buffer.len() >= buf_capacity {
                            break;
                        }
                    }

                    if shutdown.load(Ordering::SeqCst) {
                        break;
                    }

                    stream.write_all(&send_buffer).ok();
                }

                stream.shutdown(Shutdown::Both).ok();
            }
        });

        self.tx_thread = Some(handle);
    }

    pub fn add(&mut self, data: &[u8], chans: usize) {
        if let Some(producer) = self.producer.as_mut() {
            if let Some(config_ready) = &self.config_ready {
                let (lock, cvar) = &**config_ready;
                let mut config = lock.lock().unwrap();
                if config.0.is_none() {
                    config.0 = Some((data.len() / 3 / chans)); // 24-bit
                    config.1 = Some(chans);
                    cvar.notify_one();
                }
            }

            // Create a single Vec<u8> from the input data
            let data_vec = Vec::from(data);

            match producer.write_chunk_uninit(1) {
                // Write 1 chunk since we're writing one vector
                Ok(chunk) => {
                    chunk.fill_from_iter(std::iter::once(data_vec)); // Pass the whole vector as one item
                }
                Err(err) => {
                    error!("Error adding to rtrb: {}", err);
                }
            }

            let (lock, cvar) = &*self.data_available;
            let mut available = lock.lock().unwrap();
            *available = true;
            cvar.notify_one();
        }
    }

    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(config_ready) = &self.config_ready {
            let (_, cvar) = &**config_ready;
            cvar.notify_all();
        }

        let (_, cvar) = &*self.data_available;
        cvar.notify_all();
        self.tx_thread.take();
    }
}

impl Drop for AudioProcessor {
    fn drop(&mut self) {
        if !self.shutdown.load(Ordering::SeqCst) {
            self.shutdown();
        }
    }
}

#[no_mangle]
pub extern "C" fn audio_processor_new(chans: usize) -> *mut c_void {
    let processor = AudioProcessor::new(chans);
    Box::into_raw(Box::new(processor)) as *mut c_void
}

#[no_mangle]
pub extern "C" fn audio_processor_add(
    instance: *mut c_void,
    data_ptr: *const u8,
    num_frames: usize,
    num_chans: usize,
) {
    let processor: &mut AudioProcessor = unsafe {
        assert!(!instance.is_null());
        &mut *(instance as *mut AudioProcessor)
    };
    let data: &[u8] = unsafe {
        assert!(!data_ptr.is_null());
        std::slice::from_raw_parts(data_ptr, num_frames * num_chans * 3) // 3 bytes per 24-bit sample
    };

    processor.add(data, num_chans);
}

#[no_mangle]
pub extern "C" fn audio_processor_shutdown(instance: *mut c_void) {
    unsafe {
        assert!(!instance.is_null());
        (&mut *(instance as *mut AudioProcessor)).shutdown();
    }
}

#[no_mangle]
pub extern "C" fn audio_processor_destroy(instance: *mut c_void) {
    if !instance.is_null() {
        unsafe {
            println!("audio_processor_destroy called");
            drop(Box::from_raw(instance as *mut AudioProcessor));
        }
    }
}
