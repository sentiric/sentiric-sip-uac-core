use super::adapter::MediaAdapter;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct HeadlessAdapter {
    is_muted: Arc<AtomicBool>,
}

impl HeadlessAdapter {
    pub fn new() -> Self {
        Self {
            is_muted: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl MediaAdapter for HeadlessAdapter {
    fn read_mic(&self, target_8k_samples: usize) -> Vec<i16> {
        if self.is_muted.load(Ordering::Relaxed) {
            vec![0; target_8k_samples] // Sessizlik
        } else {
            // Sunucu testleri için VAD'ı tetikleyecek sahte sinyal (Pseudo-noise)
            vec![1000; target_8k_samples] 
        }
    }

    fn write_spk(&self, _samples_8k: &[i16]) {
        // Headless modunda ses boşa atılır
    }

    fn is_healthy(&self) -> bool {
        true
    }

    fn set_mute(&self, muted: bool) {
        self.is_muted.store(muted, Ordering::Relaxed);
    }

    fn update_gains(&self, _mic: f32, _spk: f32) {}
}