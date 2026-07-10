use std::error::Error as _;

use tina_http::{HttpRequest, RequestHeaderError};

#[test]
fn untrusted_header_name_cannot_inject_a_second_field() {
    let error = HttpRequest::get("/")
        .try_header("x-user\r\nx-admin", "true")
        .expect_err("CRLF in a header name must be rejected");

    assert!(matches!(&error, RequestHeaderError::InvalidName(_)));
    assert_eq!(error.to_string(), "invalid HTTP header name");
    assert!(error.source().is_some(), "parser error remains inspectable");
}

#[test]
fn untrusted_header_value_cannot_inject_a_second_field() {
    let error = HttpRequest::get("/")
        .try_header("x-user", "safe\r\nx-admin: true")
        .expect_err("CRLF in a header value must be rejected");

    assert!(matches!(&error, RequestHeaderError::InvalidValue(_)));
    assert_eq!(error.to_string(), "invalid HTTP header value");
    assert!(error.source().is_some(), "parser error remains inspectable");
}

#[test]
fn valid_untrusted_headers_are_appended_and_built() {
    let request = HttpRequest::get("/")
        .try_header("x-role", "reader")
        .expect("valid first header")
        .try_header("x-role", "auditor")
        .expect("valid repeated header")
        .build();

    let values = request
        .headers
        .get_all("x-role")
        .iter()
        .map(|value| value.to_str().expect("ASCII test value"))
        .collect::<Vec<_>>();
    assert_eq!(values, ["reader", "auditor"]);
}
