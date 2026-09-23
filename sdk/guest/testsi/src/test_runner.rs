pub type TestResult = Result<(), libtest_mimic::Failed>;

pub struct TestCase {
    pub name: &'static str,
    pub function: fn() -> TestResult,
    pub ignore: bool,
    pub kind: Option<&'static str>,
}

inventory::collect!(TestCase);

pub use inventory;
pub use libtest_mimic;
pub use testsi_macros::test_dapp;

#[macro_export]
macro_rules! testsi_main {
    () => {
        fn main() {
            let mimic_args = {
                let mut a = testsi::libtest_mimic::Arguments::default();
                a.test_threads = Some(1);
                a
            };
            let mut trials: Vec<_> = testsi::inventory::iter::<testsi::TestCase>
                .into_iter()
                .map(|c| {
                    let mut t = testsi::libtest_mimic::Trial::test(c.name, c.function)
                        .with_ignored_flag(c.ignore);

                    if let Some(k) = c.kind {
                        t = t.with_kind(k);
                    }

                    t
                })
                .collect();

            trials.sort_by(|a, b| match a.kind().cmp(b.kind()) {
                std::cmp::Ordering::Equal => a.name().cmp(b.name()),
                x => x,
            });

            testsi::libtest_mimic::run(&mimic_args, trials).exit();
        }
    };
}
