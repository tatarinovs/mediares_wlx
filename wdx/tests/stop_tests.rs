//! Separate test executable: ordinary field requests reset the DLL-global stop flag,
//! so this assertion must not race field tests running in parallel.

use mediares_wdx::ContentStopGetValueW;

#[test]
fn test_stop_get_value_cancellation() {
    assert!(!mediares_core::wdx_api::is_stop_requested());
    unsafe {
        let dummy = [0u16; 1];
        ContentStopGetValueW(dummy.as_ptr());
    }
    assert!(mediares_core::wdx_api::is_stop_requested());
    mediares_core::wdx_api::reset_stop_flag();
    assert!(!mediares_core::wdx_api::is_stop_requested());
}
