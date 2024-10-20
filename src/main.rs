use au_trx::AudioProcessor;
use gen_id::{ConfigPreset::ShortEpochMaxNodes, IdGenerator, DEFAULT_EPOCH};

fn main() {
    let gen = IdGenerator::new(ShortEpochMaxNodes, DEFAULT_EPOCH);
    let ap = AudioProcessor::new(true);

    loop {
        std::thread::park(); // Park the main thread, preventing it from consuming CPU unnecessarily
    }
}
