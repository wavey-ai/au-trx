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
use std::time::{SystemTime, UNIX_EPOCH};

const BITS_PER_SAMPLE: u8 = 24;
const RECONNECT_INTERVAL: Duration = Duration::from_millis(50);

pub struct AudioProcessor {
    tcp_port: u16,
    producer: Producer<(u64, Vec<u8>)>,
    consumer: Option<Consumer<(u64, Vec<u8>)>>,
    samples_per_channel: Option<usize>,
    shutdown: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    data_ready: Arc<(Mutex<bool>, Condvar)>,
    tx_thread: Option<JoinHandle<()>>,
    num_channels: u8,
    sample_rate: u32,
}

impl AudioProcessor {
    pub fn new(tcp_port: u16, num_channels: u8, sample_rate: u32) -> Self {
        let (producer, consumer) = RingBuffer::<(u64, Vec<u8>)>::new(256);
        Self {
            tcp_port,
            producer,
            consumer: Some(consumer),
            samples_per_channel: None,
            num_channels,
            shutdown: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
            data_ready: Arc::new((Mutex::new(false), Condvar::new())),
            tx_thread: None,
            sample_rate,
        }
    }

    fn handle_connection(
        mut stream: TcpStream,
        samples_per_channel: usize,
        num_channels: u8,
        sample_rate: u32,
        shutdown: Arc<AtomicBool>,
        consumer: &mut Consumer<(u64, Vec<u8>)>,
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
            sample_rate,
            num_channels,
            BITS_PER_SAMPLE,
            Endianness::LittleEndian,
            Some(frame_id),
            Some(123),
        )
        .unwrap();

        let mut header_data = Vec::with_capacity(header.size());
        header.encode(&mut header_data).ok();

        let frame_size = samples_per_channel * num_channels as usize * 3; // 3 bytes per sample
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

            'inner: for _ in 0..consumer.slots() {
                match consumer.pop() {
                    Ok(chunk) => {
                        FrameHeader::patch_pts(&mut header_data, Some(chunk.0)).unwrap();
                        send_buffer.clear();
                        send_buffer.extend_from_slice(&(total_size as u32).to_le_bytes());
                        send_buffer.extend_from_slice(&header_data);
                        send_buffer.extend_from_slice(&chunk.1);
                        stream.write_all(&send_buffer)?;
                    }
                    Err(e) => {
                        break 'inner;
                    }
                }
            }

            if shutdown.load(Ordering::SeqCst) {
                return Ok(());
            }
        }
    }

    pub fn add(&mut self, data: &[u8]) {
        if !self.started.load(Ordering::SeqCst) {
            let audio_data_len = data.len();
            // Each sample is 3 bytes (24-bit)
            let bytes_per_sample = 3;
            // Calculate samples per channel (total samples / number of channels)
            let total_samples = audio_data_len / bytes_per_sample;
            let spc = total_samples / self.num_channels as usize;
            self.samples_per_channel = Some(spc);
            self.start_tx(spc);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Time went backwards")
            .as_micros() as u64;

        if let Err(e) = self.producer.push((now, data.to_vec())) {
            return;
        };

        // Notify the Tx thread
        let (lock, cvar) = &*self.data_ready;
        let mut ready = lock.lock().unwrap();
        *ready = true;
        cvar.notify_one();
    }

    fn establish_connection(addr: SocketAddr) -> Option<TcpStream> {
        match TcpStream::connect(addr) {
            Ok(stream) => Some(stream),
            Err(_) => None,
        }
    }

    fn start_tx(&mut self, samples_per_channel: usize) {
        let shutdown_flag = Arc::clone(&self.shutdown);
        let data_ready = Arc::clone(&self.data_ready);
        let addr: SocketAddr = format!("127.0.0.1:{}", self.tcp_port).parse().unwrap();

        let mut consumer = self
            .consumer
            .take()
            .expect("Consumer was already taken or never existed");

        let num_channels = self.num_channels;
        let sample_rate = self.sample_rate;

        let handle = thread::spawn(move || {
            let gen = IdGenerator::new(ShortEpochMaxNodes, DEFAULT_EPOCH);
            while !shutdown_flag.load(Ordering::SeqCst) {
                match Self::establish_connection(addr) {
                    Some(stream) => {
                        if let Err(_) = Self::handle_connection(
                            stream,
                            samples_per_channel,
                            num_channels,
                            sample_rate,
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
pub extern "C" fn audio_processor_new(
    tcp_port: u16,
    channels: u8,
    sample_rate: u32,
) -> *mut c_void {
    let processor = AudioProcessor::new(tcp_port, channels, sample_rate);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    #[derive(Debug)]
    struct ReceivedFrame {
        header: FrameHeader,
        audio_data: Vec<u8>,
    }

    struct MockAudioServer {
        listener: TcpListener,
        received_frames: Arc<Mutex<Vec<ReceivedFrame>>>,
    }

    impl MockAudioServer {
        fn new(port: u16) -> Self {
            let listener =
                TcpListener::bind(format!("127.0.0.1:{}", port)).expect("Failed to bind to port");
            Self {
                listener,
                received_frames: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn start(&self) -> Arc<Mutex<Vec<ReceivedFrame>>> {
            let listener = self.listener.try_clone().unwrap();
            let frames = Arc::clone(&self.received_frames);

            thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    println!("Server: Accepted connection");

                    // Expect HELO
                    let mut hello_buf = [0u8; 4];
                    if let Ok(_) = stream.read_exact(&mut hello_buf) {
                        assert_eq!(&hello_buf, b"HELO", "Expected HELO handshake");
                        println!("Server: Received HELO");

                        // Send frame ID (u16 as little endian)
                        stream.write_all(&1u16.to_le_bytes()).unwrap();
                        println!("Server: Sent frame ID");

                        // Read frames
                        loop {
                            // Read total size
                            let mut size_buf = [0u8; 4];
                            if stream.read_exact(&mut size_buf).is_err() {
                                break;
                            }
                            let total_size = u32::from_le_bytes(size_buf) as usize;
                            println!("Server: Frame size: {}", total_size);

                            // Read frame data
                            let mut frame_data = vec![0u8; total_size - 4];
                            if stream.read_exact(&mut frame_data).is_err() {
                                break;
                            }

                            println!(
                                "Server: Raw header bytes: {:02x?}",
                                &frame_data[..std::cmp::min(frame_data.len(), 20)]
                            );

                            // Parse header
                            if let Ok(header) = FrameHeader::decode(&mut &frame_data[..]) {
                                let header_size = header.size();
                                let audio_data = frame_data[header_size..].to_vec();

                                println!("Server: Decoded frame header:");
                                println!("  Encoding: {:?}", header.encoding());
                                println!("  Sample Size: {}", header.sample_size());
                                println!("  Sample Rate: {}", header.sample_rate());
                                println!("  Channels: {}", header.channels());
                                println!("  Bits/Sample: {}", header.bits_per_sample());
                                println!("  Endianness: {:?}", header.endianness());
                                println!("  ID: {:?}", header.id());
                                println!("  PTS: {:?}", header.pts());
                                println!("  Header Size: {}", header_size);
                                println!("  Audio Data Size: {}", audio_data.len());

                                frames
                                    .lock()
                                    .unwrap()
                                    .push(ReceivedFrame { header, audio_data });
                            } else {
                                println!("Server: Failed to decode header!");
                            }
                        }
                    }
                }
            });

            Arc::clone(&self.received_frames)
        }
    }

    #[test]
    fn test_audio_processor_lifecycle() {
        const TEST_PORT: u16 = 12345;

        // Start mock server
        let server = MockAudioServer::new(TEST_PORT);
        let received_frames = server.start();

        thread::sleep(Duration::from_millis(100));

        let processor = audio_processor_new(TEST_PORT, 2, 48_000);
        assert!(!processor.is_null(), "AudioProcessor creation failed");

        let num_samples = 10;
        let mut test_data = vec![0u8; 3 * 2 * num_samples];

        println!("Client: Creating test data");
        println!("  Num samples: {}", num_samples);
        println!("  Total bytes: {}", test_data.len());

        // Fill with test pattern (24-bit stereo)
        for i in (0..test_data.len()).step_by(6) {
            // Left channel
            test_data[i] = 0x12;
            test_data[i + 1] = 0x34;
            test_data[i + 2] = 0x56;
            // Right channel
            test_data[i + 3] = 0x78;
            test_data[i + 4] = 0x9A;
            test_data[i + 5] = 0xBC;
        }

        println!(
            "Client: First few bytes of test data: {:02x?}",
            &test_data[..std::cmp::min(test_data.len(), 20)]
        );

        // Send data
        audio_processor_add(processor, test_data.as_ptr(), test_data.len());

        thread::sleep(Duration::from_millis(500));
        audio_processor_add(processor, test_data.as_ptr(), test_data.len());

        thread::sleep(Duration::from_millis(500));
        audio_processor_add(processor, test_data.as_ptr(), test_data.len());

        // Validate
        let frames = received_frames.lock().unwrap();
        assert!(!frames.is_empty(), "No frames received");

        let first_frame = &frames[0];
        //        assert_eq!(first_frame.header.pts(), Some(pts), "Incorrect PTS value");
        assert_eq!(first_frame.header.channels(), 2, "Expected stereo");
        assert_eq!(
            first_frame.header.bits_per_sample(),
            24,
            "Expected 24-bit audio"
        );
        assert_eq!(
            first_frame.header.sample_rate(),
            48_000,
            "Expected 48kHz sample rate"
        );

        // Verify audio data
        assert_eq!(
            first_frame.audio_data.len(),
            3 * 2 * num_samples,
            "Incorrect audio data length. Expected {}, got {}",
            3 * 2 * num_samples,
            first_frame.audio_data.len()
        );

        for i in (0..first_frame.audio_data.len()).step_by(6) {
            assert_eq!(
                &first_frame.audio_data[i..i + 3],
                &[0x12, 0x34, 0x56],
                "Incorrect left channel data at offset {}",
                i
            );
            assert_eq!(
                &first_frame.audio_data[i + 3..i + 6],
                &[0x78, 0x9A, 0xBC],
                "Incorrect right channel data at offset {}",
                i
            );
        }

        // Cleanup
        audio_processor_shutdown(processor);
        audio_processor_destroy(processor);
    }
}
