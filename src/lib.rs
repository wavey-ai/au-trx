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

pub struct AudioProcessor {
    producer: Option<Producer<f32>>,
    consumer: Option<Consumer<f32>>,
    data_available: Arc<(Mutex<bool>, Condvar)>,
    shutdown: Arc<AtomicBool>,
    tx_thread: Option<JoinHandle<()>>,
}

impl AudioProcessor {
    pub fn shutdown(&mut self) {
        println!("AudioProcessor::shutdown() called");
        self.shutdown.store(true, Ordering::SeqCst);

        // Wake up the thread if it's waiting on the condvar
        let (lock, cvar) = &*self.data_available;
        if let Ok(mut available) = lock.lock() {
            *available = true;
            cvar.notify_all();
        }

        // Spawn thread to handle cleanup
        if let Some(handle) = self.tx_thread.take() {
            thread::spawn(move || match handle.join() {
                Ok(_) => println!("Tx thread joined"),
                Err(_) => eprintln!("Tx thread panicked during join"),
            });
        }
    }

    pub fn new(num_channels: usize) -> Self {
        let (producer, consumer) = RingBuffer::new(1024 * 5 * 2);
        let data_available = Arc::new((Mutex::new(false), Condvar::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let mut proc = AudioProcessor {
            producer: Some(producer),
            consumer: Some(consumer),
            data_available,
            shutdown,
            tx_thread: None,
        };

        proc.start_tx(num_channels);
        proc
    }

    fn start_tx(&mut self, num_channels: usize) {
        let samples_per_channel = 1024;
        let mut consumer = self.consumer.take().unwrap();
        let data_available = Arc::clone(&self.data_available);
        let shutdown = Arc::clone(&self.shutdown);

        let handle = thread::spawn(move || {
            let pcm_capacity = samples_per_channel * num_channels * std::mem::size_of::<f32>();
            let tcp_address = "127.0.0.1:4242";
            let gen = IdGenerator::new(ShortEpochMaxNodes, DEFAULT_EPOCH);

            'outer: while !shutdown.load(Ordering::SeqCst) {
                println!("Attempting to connect to {}", tcp_address);

                let mut stream = match TcpStream::connect(tcp_address) {
                    Ok(stream) => stream,
                    Err(e) => {
                        eprintln!("Failed to connect: {}", e);
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };

                println!("Connected to server, sending HELO");

                if let Err(e) = stream.write_all(b"HELO") {
                    eprintln!("Failed to send HELO: {}", e);
                    continue;
                }

                let mut id_buffer = [0u8; 2];
                if let Err(e) = stream.read_exact(&mut id_buffer) {
                    eprintln!("Failed to receive ID: {}", e);
                    continue;
                }

                let received_id = u16::from_le_bytes(id_buffer);
                let id = gen.next_id(received_id);

                let header = FrameHeader::new(
                    EncodingFlag::PCMFloat,
                    samples_per_channel as u16,
                    48000,
                    num_channels as u8,
                    32,
                    Endianness::LittleEndian,
                    Some(id),
                );

                let mut header_data = Vec::new();
                if let Err(e) = header.encode(&mut header_data) {
                    eprintln!("Failed to encode header: {}", e);
                    continue;
                }

                println!("Connection established, starting audio transmission");

                let buf_capacity = pcm_capacity + header_data.len();
                let mut send_buffer = Vec::with_capacity(buf_capacity);
                let mut data_buffer = Vec::with_capacity(pcm_capacity);

                'inner: loop {
                    if shutdown.load(Ordering::SeqCst) {
                        break 'outer;
                    }

                    send_buffer.extend_from_slice(&header_data);

                    'buffer_fill: loop {
                        if shutdown.load(Ordering::SeqCst) {
                            break 'outer;
                        }

                        let (lock, cvar) = &*data_available;
                        let mut available = lock.lock().unwrap();

                        while !*available {
                            if shutdown.load(Ordering::SeqCst) {
                                break 'outer;
                            }
                            available = cvar.wait(available).unwrap();
                        }

                        while let Ok(sample) = consumer.pop() {
                            let bytes = sample.to_le_bytes();
                            send_buffer.extend_from_slice(&bytes);
                            data_buffer.extend_from_slice(&bytes);

                            if send_buffer.len() >= buf_capacity {
                                break 'buffer_fill;
                            }
                        }

                        *available = false;
                    }

                    if let Err(e) = stream.write_all(&send_buffer) {
                        eprintln!("Failed to send audio data: {}", e);
                        break;
                    }

                    send_buffer.clear();
                    data_buffer.clear();
                }

                let _ = stream.shutdown(Shutdown::Both);
            }

            println!("Tx thread exiting");
        });

        self.tx_thread = Some(handle);
    }

    pub fn add(&mut self, data: &[f32]) {
        if let Some(producer) = self.producer.as_mut() {
            match producer.write_chunk_uninit(data.len()) {
                Ok(chunk) => {
                    chunk.fill_from_iter(data.iter().cloned());
                    println!("adding to rtrb: {}", data.len());

                    // Signal that new data is available
                    let (lock, cvar) = &*self.data_available;
                    let mut available = lock.lock().unwrap();
                    *available = true;
                    cvar.notify_one();
                }
                Err(e) => {
                    eprintln!("Error adding to rtrb: {}", e);
                }
            }
        }
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
    data_ptr: *const f32,
    num_samples: usize,
) {
    let processor = unsafe {
        assert!(!instance.is_null());
        &mut *(instance as *mut AudioProcessor)
    };
    let data = unsafe {
        assert!(!data_ptr.is_null());
        std::slice::from_raw_parts(data_ptr, num_samples)
    };

    processor.add(data);
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
