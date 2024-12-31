use frame_header::{EncodingFlag, Endianness, FrameHeader};
use gen_id::{ConfigPreset::ShortEpochMaxNodes, IdGenerator, DEFAULT_EPOCH};
use rtrb::{Consumer, Producer, RingBuffer};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const BITS_PER_SAMPLE: u8 = 24;
const RECONNECT_INTERVAL: Duration = Duration::from_millis(50);

pub struct AudioProcessor {
    tcp_port: u16,
    producer: Producer<u8>,
    consumer: Option<Consumer<u8>>,
    frame_size: Option<usize>, // Size of one complete audio frame in bytes
    samples_per_channel: Option<usize>,
    num_channels: Option<usize>,
    shutdown: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    data_ready: Arc<(Mutex<bool>, Condvar)>,
    tx_thread: Option<JoinHandle<()>>,
}

impl AudioProcessor {
    pub fn new(tcp_port: u16, ring_buffer_bytes: usize) -> Self {
        let (producer, consumer) = RingBuffer::<u8>::new(ring_buffer_bytes);
        Self {
            tcp_port,
            producer,
            consumer: Some(consumer),
            frame_size: None,
            samples_per_channel: None,
            num_channels: None,
            shutdown: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
            data_ready: Arc::new((Mutex::new(false), Condvar::new())),
            tx_thread: None,
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        samples_per_channel: usize,
        num_channels: usize,
        shutdown: Arc<AtomicBool>,
        consumer: &mut Consumer<u8>,
        data_ready: Arc<(Mutex<bool>, Condvar)>,
        gen: &IdGenerator,
    ) -> Result<(), std::io::Error> {
        stream.write_all(b"HELO")?;

        let mut id_buf = [0u8; 2];
        stream.read_exact(&mut id_buf)?;
        let frame_id = gen.next_id(u16::from_le_bytes(id_buf));

        let header = FrameHeader::new(
            EncodingFlag::PCMSigned,
            samples_per_channel as u16,
            48_000,
            num_channels as u8,
            BITS_PER_SAMPLE,
            Endianness::LittleEndian,
            Some(frame_id),
        )
        .unwrap();

        let mut header_data = Vec::with_capacity(header.size());
        header.encode(&mut header_data).ok();

        let frame_size = samples_per_channel * num_channels * 3; // 3 bytes per sample
        let total_size = frame_size + header_data.len() + 4;
        let mut send_buffer = Vec::with_capacity(total_size);

        loop {
            if shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }

            // Wait for data or shutdown
            {
                let (lock, cvar) = &*data_ready;
                let mut ready = lock.lock().unwrap();
                while !*ready && !shutdown.load(Ordering::SeqCst) {
                    let _ = cvar.wait_timeout(ready, RECONNECT_INTERVAL).unwrap();
                    ready = lock.lock().unwrap();
                }
                *ready = false;
            }

            if shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }

            send_buffer.clear();
            send_buffer.extend_from_slice(&(total_size as u32).to_le_bytes());
            send_buffer.extend_from_slice(&header_data);

            // Read exactly one frame's worth of data
            if let Ok(chunk) = consumer.read_chunk(frame_size) {
                let (first, second) = chunk.as_slices();
                send_buffer.extend_from_slice(first);
                send_buffer.extend_from_slice(second);
                chunk.commit_all();

                if shutdown.load(Ordering::SeqCst) {
                    return Ok(());
                }
                stream.write_all(&send_buffer)?;
            }
        }
    }

    pub fn add(&mut self, data: &[u8]) {
        if !self.started.load(Ordering::SeqCst) {
            let frames = data.len() / 3; // 3 bytes per sample
            let nc = self.num_channels.unwrap_or(1);
            self.num_channels = Some(nc);
            let spc = frames / nc;
            self.samples_per_channel = Some(spc);
            self.frame_size = Some(frames * 3);
            self.start_tx(spc, nc);
        }

        // Write entire chunk at once using the safe write_chunk API
        if let Ok(mut chunk) = self.producer.write_chunk(data.len()) {
            let (first, second) = chunk.as_mut_slices();
            let first_len = first.len();
            first.copy_from_slice(&data[..first_len]);
            if !second.is_empty() {
                second.copy_from_slice(&data[first_len..]);
            }
            chunk.commit_all();

            // Notify the Tx thread
            let (lock, cvar) = &*self.data_ready;
            let mut ready = lock.lock().unwrap();
            *ready = true;
            cvar.notify_one();
        }
    }

    fn establish_connection(addr: SocketAddr) -> Option<TcpStream> {
        match TcpStream::connect(addr) {
            Ok(stream) => Some(stream),
            Err(_) => None,
        }
    }

    fn start_tx(&mut self, samples_per_channel: usize, num_channels: usize) {
        let shutdown_flag = Arc::clone(&self.shutdown);
        let data_ready = Arc::clone(&self.data_ready);
        let addr: SocketAddr = format!("127.0.0.1:{}", self.tcp_port).parse().unwrap();

        let mut consumer = self
            .consumer
            .take()
            .expect("Consumer was already taken or never existed");

        let handle = thread::spawn(move || {
            let gen = IdGenerator::new(ShortEpochMaxNodes, DEFAULT_EPOCH);
            while !shutdown_flag.load(Ordering::SeqCst) {
                match Self::establish_connection(addr) {
                    Some(stream) => {
                        if let Err(_) = Self::handle_connection(
                            stream,
                            samples_per_channel,
                            num_channels,
                            Arc::clone(&shutdown_flag),
                            &mut consumer,
                            Arc::clone(&data_ready),
                            &gen,
                        ) {
                            // On error, sleep briefly and reconnect
                            thread::sleep(RECONNECT_INTERVAL);
                        }
                    }
                    None => {
                        // If connect failed, sleep and retry
                        thread::sleep(RECONNECT_INTERVAL);
                    }
                }
            }
        });

        self.tx_thread = Some(handle);
        self.started.store(true, Ordering::SeqCst);
    }

    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        // Notify in case Tx thread is waiting
        let (_, cvar) = &*self.data_ready;
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
pub extern "C" fn audio_processor_new(tcp_port: u16, ring_bytes: usize) -> *mut c_void {
    let processor = AudioProcessor::new(tcp_port, ring_bytes);
    Box::into_raw(Box::new(processor)) as *mut c_void
}

#[no_mangle]
pub extern "C" fn audio_processor_add(instance: *mut c_void, data_ptr: *const u8, length: usize) {
    unsafe {
        let processor = &mut *(instance as *mut AudioProcessor);
        let data = std::slice::from_raw_parts(data_ptr, length);
        processor.add(data);
    }
}

#[no_mangle]
pub extern "C" fn audio_processor_shutdown(instance: *mut c_void) {
    unsafe {
        let processor = &mut *(instance as *mut AudioProcessor);
        processor.shutdown();
    }
}

#[no_mangle]
pub extern "C" fn audio_processor_destroy(instance: *mut c_void) {
    if !instance.is_null() {
        unsafe {
            drop(Box::from_raw(instance as *mut AudioProcessor));
        }
    }
}
