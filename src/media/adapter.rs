pub trait MediaAdapter: Send + Sync {
    /// Ağdan göndermek üzere mikrofondan belirtilen sayıda PCM örneği okur.
    fn read_mic(&self, target_8k_samples: usize) -> Vec<i16>;
    
    /// Ağdan alınan PCM örneklerini hoparlöre yazar.
    fn write_spk(&self, samples_8k: &[i16]);

    /// Adaptörün (donanımın) hala ayakta olup olmadığını kontrol eder.
    fn is_healthy(&self) -> bool;

    /// Mikrofonu susturur (sessizlik basar).
    fn set_mute(&self, muted: bool);

    /// Yazılımsal ses şiddetini günceller.
    fn update_gains(&self, mic: f32, spk: f32);
}