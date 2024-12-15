use au_trx::AudioProcessor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

fn f32_to_i24(sample: f32) -> [u8; 3] {
    // Convert float (-1.0 to 1.0) to 24-bit integer
    let int_val = (sample * 8388607.0) as i32; // 2^23 - 1 = 8388607
    let bytes = int_val.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]] // Take first 3 bytes for 24-bit
}

fn main() {
    // Create AudioProcessor with 2 channels
    let mut processor = AudioProcessor::new(2);

    // Audio parameters
    let sample_rate = 48000.0;
    let frequency = 261.63; // Middle C
    let frames = 1024;
    let num_channels = 2;
    let mut phase = 0.0;

    // Run for a fixed duration (e.g., 10 seconds)
    let duration_secs = 100;
    let start_time = std::time::Instant::now();

    while start_time.elapsed() < Duration::from_secs(duration_secs) {
        // Generate 1024 frames of stereo audio
        let mut pcm_data = Vec::with_capacity(frames * num_channels * 3); // 3 bytes per sample

        for i in 0..frames {
            // Calculate the next sample
            let t = phase + (i as f32 / sample_rate);
            let value = (2.0 * std::f32::consts::PI * frequency * t).sin();

            // Convert to 24-bit samples for both channels
            let bytes = f32_to_i24(value * 0.9); // Slight headroom
            pcm_data.extend_from_slice(&bytes); // Left channel
            pcm_data.extend_from_slice(&bytes); // Right channel
        }

        // Update phase to maintain continuity
        phase += frames as f32 / sample_rate;
        if phase >= 1.0 {
            phase -= 1.0;
        }

        // Send the PCM data to the processor
        processor.add(&pcm_data, num_channels);

        // Small sleep to prevent busy-waiting
        thread::sleep(Duration::from_millis(10));
    }

    // Clean shutdown
    println!("Shutting down...");
    processor.shutdown();
    println!("Shutdown complete.");
}
