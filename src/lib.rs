use gen_id::{ConfigPreset::ShortEpochMaxNodes, IdGenerator, DEFAULT_EPOCH};
use rtrb::{Consumer, Producer, RingBuffer};
use soundkit::{
    audio_packet::FrameHeader,
    audio_types::{EncodingFlag, Endianness},
};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use std::fs::OpenOptions;
use std::io::Result;

fn save_as_pcm(data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .append(true) // Open the file in append mode
        .create(true) // Create the file if it doesn’t exist
        .open("test.pcm")?;
    file.write_all(data)?;
    Ok(())
}

pub struct AudioProcessor {
    producer: Option<Producer<f32>>,
    consumer: Option<Consumer<f32>>,
}

impl AudioProcessor {
    pub fn new(tx: bool) -> Self {
        let (producer, consumer) = RingBuffer::new(1024 * 5 * 2);
        let mut proc = AudioProcessor {
            producer: Some(producer),
            consumer: Some(consumer),
        };

        if tx {
            proc.start_tx(2)
        } else {
            proc.start_rx(2)
        }
        proc
    }

    pub fn add(&mut self, data: &[f32]) {
        if let Some(producer) = self.producer.as_mut() {
            match producer.write_chunk_uninit(data.len()) {
                Ok(chunk) => {
                    chunk.fill_from_iter(data.iter().cloned());
                    println!("adding to rtrb: {}", data.len());
                }
                Err(e) => {
                    eprintln!("Error adding to rtrb: {}", e);
                }
            }
        }
    }

    pub fn start_tx(&mut self, num_channels: usize) {
        let samples_per_channel = 1024;
        let mut consumer = self.consumer.take().unwrap();
        thread::spawn(move || {
            let pcm_capacity = samples_per_channel * num_channels * std::mem::size_of::<f32>();
            let tcp_address = "127.0.0.1:4242";
            let mut stream = match TcpStream::connect(tcp_address) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to connect to the server: {}", e);
                    return;
                }
            };

            if let Err(e) = stream.write_all(b"HELO") {
                eprintln!("Failed to send HELO message: {}", e);
                return;
            }

            let mut id_buffer = [0u8; 2];
            if let Err(e) = stream.read_exact(&mut id_buffer) {
                eprintln!("Failed to receive ID from server: {}", e);
                return;
            }

            let received_id = u16::from_le_bytes(id_buffer);
            let gen = IdGenerator::new(ShortEpochMaxNodes, DEFAULT_EPOCH);
            let id = gen.next_id(received_id as u16);

            let header = FrameHeader::new(
                EncodingFlag::PCMFloat,
                samples_per_channel as u16,
                48000,
                2,
                32,
                Endianness::LittleEndian,
                Some(id),
            );
            let mut header_data: Vec<u8> = Vec::new();
            header
                .encode(&mut header_data)
                .expect("Failed to encode header");

            let buf_capacity = pcm_capacity + header_data.len();
            let mut send_buffer = Vec::with_capacity(buf_capacity);

            let mut data_buffer = Vec::with_capacity(pcm_capacity);

            loop {
                send_buffer.extend_from_slice(&header_data);
                while send_buffer.len() < buf_capacity {
                    if let Ok(sample) = consumer.pop() {
                        let bytes = sample.to_le_bytes();
                        send_buffer.extend_from_slice(&bytes);
                        data_buffer.extend_from_slice(&bytes);
                    } else {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                if let Err(e) = stream.write_all(&send_buffer) {
                    eprintln!("Failed to send audio data: {}", e);
                    break;
                }
                send_buffer.clear();
                data_buffer.clear();

                if let Err(e) = save_as_pcm(&data_buffer) {
                    eprintln!("Failed to save PCM file: {}", e);
                } else {
                    println!("PCM file saved successfully as test.pcm");
                }
            }
        });
    }

    pub fn start_rx(&mut self, num_channels: usize) {
        let mut producer = self.producer.take().unwrap();

        thread::spawn(move || {
            let tcp_address = "127.0.0.1:4242";
            let mut stream = match TcpStream::connect(tcp_address) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to connect to the server for receiving: {}", e);
                    return;
                }
            };
            if let Err(e) = stream.write_all(b"play") {
                eprintln!("Failed to send audio data: {}", e);
            }

            let len: usize = 4 * num_channels;
            let mut buffer = vec![0u8; len];
            loop {
                match stream.read_exact(&mut buffer) {
                    Ok(_) => {
                        for chunk in buffer.chunks_exact(4) {
                            let sample =
                                f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                            match producer.push(sample) {
                                Ok(_) => {}
                                Err(e) => eprintln!("Error adding received data to buffer: {}", e),
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to receive audio data: {}", e);
                        break;
                    }
                }
            }
        });
    }

    pub fn pop_sample(&mut self) -> Option<f32> {
        self.consumer
            .as_mut()
            .and_then(|consumer| consumer.pop().ok())
    }

    pub fn pop_samples(&mut self, num_samples: usize) -> Vec<f32> {
        return self.inner_pop_samples(num_samples);
        loop {
            let samples = self.inner_pop_samples(num_samples);
            if samples.len() > 0 {
                return samples;
            }
        }
    }

    fn inner_pop_samples(&mut self, num_samples: usize) -> Vec<f32> {
        let mut buffer = vec![0.0f32; num_samples];
        if let Some(consumer) = self.consumer.as_mut() {
            match consumer.read_chunk(num_samples) {
                Ok(chunk) => {
                    for (i, sample) in chunk.into_iter().enumerate() {
                        buffer[i] = sample;
                    }
                    buffer
                }
                Err(e) => buffer,
            }
        } else {
            buffer
        }
    }
}

#[no_mangle]
pub extern "C" fn audio_processor_pop_samples(
    instance: *mut c_void,
    num_samples: usize,
    out_samples: *mut f32,
) -> usize {
    let processor = unsafe {
        assert!(!instance.is_null());
        &mut *(instance as *mut AudioProcessor)
    };

    let samples = processor.pop_samples(num_samples);

    if samples.is_empty() {
        return 0; // No samples available, return 0
    }

    unsafe {
        std::ptr::copy_nonoverlapping(samples.as_ptr(), out_samples, samples.len());
    }

    samples.len() // Return the number of samples retrieved
}

#[no_mangle]
pub extern "C" fn audio_processor_new(tx: bool) -> *mut c_void {
    let processor = AudioProcessor::new(tx);
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
pub extern "C" fn audio_processor_destroy(instance: *mut c_void) {
    if !instance.is_null() {
        unsafe {
            Box::from_raw(instance as *mut AudioProcessor); // This will automatically free the memory
        }
    }
}

#[no_mangle]
pub extern "C" fn audio_processor_pop(instance: *mut c_void) -> f32 {
    let processor = unsafe {
        assert!(!instance.is_null());
        &mut *(instance as *mut AudioProcessor)
    };

    processor.pop_sample().unwrap_or(0.0) // Return 0.0 if there's no sample available
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn test_pop_samples() {
        // Initialize the AudioProcessor in tx (transmit) mode
        let mut processor = AudioProcessor::new(false);
        loop {
            let start_time = Instant::now();
            let output_data = processor.pop_samples(1024 * 2);
            dbg!(output_data);
            let elapsed_time = start_time.elapsed().as_millis();
            println!("Time for this loop iteration: {} ms", elapsed_time);
        }
    }
}
