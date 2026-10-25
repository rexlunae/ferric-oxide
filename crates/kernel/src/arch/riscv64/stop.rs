use crate::sync::CleanLockToken;

pub unsafe fn kreset() -> ! {
    crate::println!("kreset");
    unimplemented!()
}

pub unsafe fn emergency_reset() -> ! {
    unimplemented!()
}

pub unsafe fn kstop(_token: &mut CleanLockToken) -> ! {
    crate::println!("kstop");
    unimplemented!()
}
