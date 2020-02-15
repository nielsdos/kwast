/// Per-CPU data.
#[repr(align(128))] // 128 = false sharing threshold
pub struct CpuData {
    /// Self reference.
    reference: usize,
}

impl CpuData {
    /// Creates a new empty per-CPU data.
    pub const fn new() -> Self {
        Self {
            // Need to fill in once we know the address.
            reference: 0,
        }
    }

    /// Prepare to set the per-CPU data.
    pub fn prepare_to_set(&mut self) {
        debug_assert_eq!(self.reference, 0);
        self.reference = self as *mut _ as usize;
    }
}
