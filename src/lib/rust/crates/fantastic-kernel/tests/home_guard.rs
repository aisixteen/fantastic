//! A disk bootstrap refuses a workdir whose `.fantastic` IS the fantastic home.
//! Its own test binary: it sets `FANTASTIC_HOME` for the process.

use fantastic_kernel::bootstrap::{bootstrap, BootstrapOptions};
use fantastic_kernel::{BundleRegistry, KernelError};

#[test]
fn a_kernel_booted_in_the_homes_parent_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let home = tmp.path().join(".fantastic");
    std::env::set_var("FANTASTIC_HOME", &home);
    for opts in [
        BootstrapOptions::daemon(tmp.path()),
        BootstrapOptions::one_shot(tmp.path()),
    ] {
        match bootstrap(BundleRegistry::new(), opts) {
            Err(KernelError::HomeAsStateDir { dir, home: h }) => {
                assert_eq!(dir, tmp.path());
                assert_eq!(h, home);
            }
            Err(e) => panic!("refused for the wrong reason: {e}"),
            Ok(_) => panic!("booted with the home as its state dir"),
        }
    }
    assert!(!home.exists(), "nothing was written into the home");
}
