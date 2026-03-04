use revm::primitives::Address;

/// Whether this frame should attempt JIT dispatch.
#[inline]
pub fn should_lookup_jit(
    frame_is_create: bool,
    bytecode_address: Option<Address>,
    bytecode_is_empty: bool,
) -> bool {
    !frame_is_create && bytecode_address.is_some() && !bytecode_is_empty
}

#[cfg(test)]
mod tests {
    use super::should_lookup_jit;
    use revm::primitives::Address;

    #[test]
    fn skips_create() {
        let addr = Address::new([0x11; 20]);
        assert!(!should_lookup_jit(true, Some(addr), false));
    }

    #[test]
    fn skips_no_bytecode_address_or_empty_bytecode() {
        let addr = Address::new([0x22; 20]);
        assert!(!should_lookup_jit(false, None, false));
        assert!(!should_lookup_jit(false, Some(addr), true));
    }

    #[test]
    fn looks_up_contract_runtime_code() {
        let addr = Address::new([0x33; 20]);
        assert!(should_lookup_jit(false, Some(addr), false));
    }
}
