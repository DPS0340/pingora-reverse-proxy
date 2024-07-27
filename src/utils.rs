use std::fmt::Debug;

use log::info;
use pingora::Error;

pub fn log_and_return_err<T: Debug>(e: Result<Box<T>, Box<Error>>) -> Result<Box<T>, Box<Error>> {
    info!("An error occurred: {e:?}");
    e
}
