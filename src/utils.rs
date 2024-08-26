use std::fmt::Debug;

use http::Uri;
use itertools::Itertools;
use log::info;
use pingora::Error;

pub fn log_and_return_err<T: Debug>(e: Result<Box<T>, Box<Error>>) -> Result<Box<T>, Box<Error>> {
    info!("An error occurred: {e:?}");
    e
}

pub fn parse_prefix(uri: &Uri) -> Result<Box<String>, Box<Error>> {
    let path = uri.path();
    let prefixes = path
        .split("/")
        .collect_vec()
        .iter()
        .enumerate()
        .filter(|(i, _)| (*i as i32) > 0)
        .map(|(_, &e)| e)
        .collect_vec();

    info!("{:?}", prefixes);

    if prefixes.len() < 2 {
        return log_and_return_err(Err(pingora::Error::explain(
            pingora::ErrorType::HTTPStatus(400),
            format!("Prefixes too short: {:?}", prefixes),
        )));
    }

    Ok(Box::new(format!("/{}/{}", prefixes[0], prefixes[1])))
}
