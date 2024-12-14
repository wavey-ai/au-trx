use au_trx::AudioProcessor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn main() {
    // Create a flag to handle Ctrl+C gracefully
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Create AudioProcessor with 2 channels
    let mut processor = AudioProcessor::new(2);

    // Generate some test data (2 channels of sine waves)
    let sample_rate = 48000.0;
    let frequency = 440.0; // A4 note
    let duration = 0.1; // 100ms chunks
    let num_samples = (sample_rate * duration) as usize * 2; // *2 for stereo

    let mut samples = Vec::with_capacity(num_samples);
    for i in 0..num_samples / 2 {
        let t = i as f32 / sample_rate;
        let value = (2.0 * std::f32::consts::PI * frequency * t).sin();

        // Add same value to both channels
        samples.push(value); // Left channel
        samples.push(value); // Right channel
    }

    // Send the data to the processor
    processor.add(&samples);

    // Wait a bit before sending next chunk
    thread::sleep(Duration::from_millis(100));
    // processor will be dropped here, testing the shutdown functionality
    dbg!("123");
    drop(processor);
    dbg!("456");

    println!("Shutdown complete.");
}
