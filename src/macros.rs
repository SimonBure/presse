macro_rules! verbose {
    ($cond:expr, $($arg:tt)*) => {
        if $cond {
            eprintln!($($arg)*);
        }
    };
}
