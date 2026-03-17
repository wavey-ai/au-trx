use frame_header::{EncodingFlag, Endianness, FrameHeader};
use rtrb::{Consumer, Producer, RingBuffer};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const BITS_PER_SAMPLE: u8 = 24;
const RECONNECT_INTERVAL: Duration = Duration::from_millis(50);
const RING_SIZE: usize = 256;

pub struct AudioProcessor {
    socket_path: String,
    data_producer: Producer<(u64, Vec<u8>)>,
    data_consumer: Option<Consumer<(u64, Vec<u8>)>>,
    free_consumer: Consumer<Vec<u8>>,
    free_producer: Option<Producer<Vec<u8>>>,
    samples_per_channel: Option<usize>,
    shutdown: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    data_ready: Arc<(Mutex<bool>, Condvar)>,
    tx_thread: Option<JoinHandle<()>>,
    num_channels: u8,
    sample_rate: u32,
    frame_id: Option<u16>,
}

impl AudioProcessor {
    pub fn new(socket_path: String, num_channels: u8, sample_rate: u32) -> Self {
        let (data_producer, data_consumer) = RingBuffer::<(u64, Vec<u8>)>::new(RING_SIZE);
        let (free_producer, free_consumer) = RingBuffer::<Vec<u8>>::new(RING_SIZE);
        Self {
            socket_path,
            data_producer,
            data_consumer: Some(data_consumer),
            free_consumer,
            free_producer: Some(free_producer),
            samples_per_channel: None,
            num_channels,
            shutdown: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
            data_ready: Arc::new((Mutex::new(false), Condvar::new())),
            tx_thread: None,
            sample_rate,
            frame_id: None,
        }
    }

    pub fn with_frame_id(mut self, frame_id: u16) -> Self {
        self.frame_id = Some(frame_id);
        self
    }

    fn handle_connection(
        mut stream: UnixStream,
        samples_per_channel: usize,
        num_channels: u8,
        sample_rate: u32,
        shutdown: Arc<AtomicBool>,
        consumer: &mut Consumer<(u64, Vec<u8>)>,
        free_producer: &mut Producer<Vec<u8>>,
        data_ready: Arc<(Mutex<bool>, Condvar)>,
        frame_id: Option<u16>,
    ) -> Result<(), std::io::Error> {
        stream.write_all(b"HELO")?;

        let mut id_buf = [0u8; 2];
        stream.read_exact(&mut id_buf)?;

        let id = if let Some(frame_id) = frame_id {
            frame_id
        } else {
            u16::from_le_bytes(id_buf)
        } as u64;

        let header = FrameHeader::new(
            EncodingFlag::PCMSigned,
            samples_per_channel as u16,
            sample_rate,
            num_channels,
            BITS_PER_SAMPLE,
            Endianness::LittleEndian,
            Some(id),
            Some(123),
        )
        .unwrap();

        let mut header_data = Vec::with_capacity(header.size());
        header.encode(&mut header_data).ok();

        let frame_size = samples_per_channel * num_channels as usize * 3; // 3 bytes per sample
        let total_size = frame_size + header_data.len() + 4;
        let mut send_buffer = Vec::with_capacity(total_size);

        loop {
            if shutdown.load(Ordering::Acquire) {
                return Ok(());
            }

            // Wait for data or shutdown
            {
                let (lock, cvar) = &*data_ready;
                let mut ready = lock.lock().unwrap();
                while !*ready && !shutdown.load(Ordering::Acquire) {
                    let (guard, _) = cvar.wait_timeout(ready, RECONNECT_INTERVAL).unwrap();
                    ready = guard;
                }
                *ready = false;
            }

            if shutdown.load(Ordering::Acquire) {
                return Ok(());
            }

            'inner: for _ in 0..consumer.slots() {
                match consumer.pop() {
                    Ok((ts, buf)) => {
                        FrameHeader::patch_pts(&mut header_data, Some(ts)).unwrap();
                        send_buffer.clear();
                        send_buffer.extend_from_slice(&(total_size as u32).to_le_bytes());
                        send_buffer.extend_from_slice(&header_data);
                        send_buffer.extend_from_slice(&buf);
                        let result = stream.write_all(&send_buffer);
                        // Return buffer to free pool before propagating any error
                        free_producer.push(buf).ok();
                        result?;
                    }
                    Err(_) => {
                        break 'inner;
                    }
                }
            }

            if shutdown.load(Ordering::Acquire) {
                return Ok(());
            }
        }
    }

    pub fn add(&mut self, data: &[u8], ts: u64) {
        if !self.started.load(Ordering::Acquire) {
            let frame_size = data.len();
            let spc = frame_size / (self.num_channels as usize * 3);
            self.samples_per_channel = Some(spc);
            self.start_tx(frame_size);
        }

        // Pop a pre-allocated buffer from the free pool; fall back to a fresh allocation
        // only if the pool is exhausted (tx thread is lagging).
        let mut buf = match self.free_consumer.pop() {
            Ok(mut b) => {
                b.clear();
                b
            }
            Err(_) => Vec::with_capacity(data.len()),
        };
        buf.extend_from_slice(data);

        if self.data_producer.push((ts, buf)).is_err() {
            return;
        }

        // Notify tx thread (skip syscall if already signaled)
        let (lock, cvar) = &*self.data_ready;
        let mut ready = lock.lock().unwrap();
        if !*ready {
            *ready = true;
            cvar.notify_one();
        }
    }

    fn establish_connection(socket_path: &str) -> Option<UnixStream> {
        match UnixStream::connect(socket_path) {
            Ok(stream) => Some(stream),
            Err(_) => None,
        }
    }

    fn start_tx(&mut self, frame_size: usize) {
        let shutdown_flag = Arc::clone(&self.shutdown);
        let data_ready = Arc::clone(&self.data_ready);
        let socket_path = self.socket_path.clone();

        let mut data_consumer = self
            .data_consumer
            .take()
            .expect("Consumer was already taken or never existed");

        // Pre-fill the free pool with correctly sized buffers before the tx thread starts,
        // so the audio thread never needs to allocate in the steady state.
        let mut free_producer = self
            .free_producer
            .take()
            .expect("free_producer was already taken");
        for _ in 0..RING_SIZE {
            free_producer.push(vec![0u8; frame_size]).ok();
        }

        let samples_per_channel = frame_size / (self.num_channels as usize * 3);
        let num_channels = self.num_channels;
        let sample_rate = self.sample_rate;
        let frame_id = self.frame_id;

        let handle = thread::spawn(move || {
            while !shutdown_flag.load(Ordering::Acquire) {
                match Self::establish_connection(&socket_path) {
                    Some(stream) => {
                        if let Err(_) = Self::handle_connection(
                            stream,
                            samples_per_channel,
                            num_channels,
                            sample_rate,
                            Arc::clone(&shutdown_flag),
                            &mut data_consumer,
                            &mut free_producer,
                            Arc::clone(&data_ready),
                            frame_id,
                        ) {
                            thread::sleep(RECONNECT_INTERVAL);
                        }
                    }
                    None => {
                        thread::sleep(RECONNECT_INTERVAL);
                    }
                }
            }
        });

        self.tx_thread = Some(handle);
        self.started.store(true, Ordering::Release);
    }

    pub fn shutdown(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        // Notify in case Tx thread is waiting
        let (_, cvar) = &*self.data_ready;
        cvar.notify_all();
        self.tx_thread.take();
    }
}

impl Drop for AudioProcessor {
    fn drop(&mut self) {
        if !self.shutdown.load(Ordering::Acquire) {
            self.shutdown();
        }
    }
}

/// Creates an AudioProcessor using a Unix domain socket path.
/// `socket_path` must be a valid UTF-8 C string (null-terminated).
#[no_mangle]
pub extern "C" fn audio_processor_new(
    socket_path: *const std::ffi::c_char,
    channels: u8,
    sample_rate: u32,
) -> *mut c_void {
    let path = unsafe { std::ffi::CStr::from_ptr(socket_path) }
        .to_string_lossy()
        .into_owned();
    let processor = AudioProcessor::new(path, channels, sample_rate);
    Box::into_raw(Box::new(processor)) as *mut c_void
}

#[no_mangle]
pub extern "C" fn audio_processor_new_with_id(
    socket_path: *const std::ffi::c_char,
    channels: u8,
    sample_rate: u32,
    frame_id: u16,
) -> *mut c_void {
    let path = unsafe { std::ffi::CStr::from_ptr(socket_path) }
        .to_string_lossy()
        .into_owned();
    let processor = AudioProcessor::new(path, channels, sample_rate).with_frame_id(frame_id);
    Box::into_raw(Box::new(processor)) as *mut c_void
}

#[no_mangle]
pub extern "C" fn audio_processor_add(
    instance: *mut c_void,
    data_ptr: *const u8,
    length: usize,
    ts: u64,
) {
    unsafe {
        let processor = &mut *(instance as *mut AudioProcessor);
        let data = std::slice::from_raw_parts(data_ptr, length);
        processor.add(data, ts);
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
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Debug)]
    struct ReceivedFrame {
        header: FrameHeader,
        audio_data: Vec<u8>,
    }

    struct MockAudioServer {
        listener: UnixListener,
        socket_path: String,
        received_frames: Arc<Mutex<Vec<ReceivedFrame>>>,
    }

    impl MockAudioServer {
        fn new(socket_path: &str) -> Self {
            let _ = std::fs::remove_file(socket_path);
            let listener = UnixListener::bind(socket_path).expect("Failed to bind socket");
            Self {
                listener,
                socket_path: socket_path.to_string(),
                received_frames: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn start(&self) -> Arc<Mutex<Vec<ReceivedFrame>>> {
            let listener = self.listener.try_clone().unwrap();
            let frames = Arc::clone(&self.received_frames);

            thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    println!("Server: Accepted connection");

                    let mut hello_buf = [0u8; 4];
                    if let Ok(_) = stream.read_exact(&mut hello_buf) {
                        assert_eq!(&hello_buf, b"HELO", "Expected HELO handshake");
                        println!("Server: Received HELO");

                        stream.write_all(&1u16.to_le_bytes()).unwrap();
                        println!("Server: Sent frame ID");

                        loop {
                            let mut size_buf = [0u8; 4];
                            if stream.read_exact(&mut size_buf).is_err() {
                                break;
                            }
                            let total_size = u32::from_le_bytes(size_buf) as usize;

                            let mut frame_data = vec![0u8; total_size - 4];
                            if stream.read_exact(&mut frame_data).is_err() {
                                break;
                            }

                            if let Ok(header) = FrameHeader::decode(&mut &frame_data[..]) {
                                let header_size = header.size();
                                let audio_data = frame_data[header_size..].to_vec();
                                frames
                                    .lock()
                                    .unwrap()
                                    .push(ReceivedFrame { header, audio_data });
                            }
                        }
                    }
                }
            });

            Arc::clone(&self.received_frames)
        }
    }

    impl Drop for MockAudioServer {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket_path);
        }
    }

    #[test]
    fn test_audio_processor_lifecycle() {
        const SOCKET_PATH: &str = "/tmp/au-trx-test.sock";

        let server = MockAudioServer::new(SOCKET_PATH);
        let received_frames = server.start();

        thread::sleep(Duration::from_millis(100));

        let socket_path_c = std::ffi::CString::new(SOCKET_PATH).unwrap();
        let processor = audio_processor_new(socket_path_c.as_ptr(), 2, 48_000);
        assert!(!processor.is_null(), "AudioProcessor creation failed");

        let num_samples = 10;
        let mut test_data = vec![0u8; 3 * 2 * num_samples];

        for i in (0..test_data.len()).step_by(6) {
            test_data[i] = 0x12;
            test_data[i + 1] = 0x34;
            test_data[i + 2] = 0x56;
            test_data[i + 3] = 0x78;
            test_data[i + 4] = 0x9A;
            test_data[i + 5] = 0xBC;
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        audio_processor_add(processor, test_data.as_ptr(), test_data.len(), now);
        thread::sleep(Duration::from_millis(500));
        audio_processor_add(processor, test_data.as_ptr(), test_data.len(), now);
        thread::sleep(Duration::from_millis(500));
        audio_processor_add(processor, test_data.as_ptr(), test_data.len(), now);

        let frames = received_frames.lock().unwrap();
        assert!(!frames.is_empty(), "No frames received");

        let first_frame = &frames[0];
        assert_eq!(first_frame.header.channels(), 2, "Expected stereo");
        assert_eq!(first_frame.header.bits_per_sample(), 24, "Expected 24-bit audio");
        assert_eq!(first_frame.header.sample_rate(), 48_000, "Expected 48kHz sample rate");
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

        audio_processor_shutdown(processor);
        audio_processor_destroy(processor);
    }
}
