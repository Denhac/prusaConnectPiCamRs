//! The little of HTTP/1.1 the API needs: GET and HEAD requests with no body, each answered in
//! full on a connection that then closes.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};

const MAX_LINE: u64 = 8 * 1024;
const MAX_HEADERS: usize = 64;

pub struct Request {
    pub method: String,
    pub path: String,
    query: Vec<(String, String)>,
    /// Lower-cased names.
    headers: Vec<(String, String)>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// Reads a request's head, or `None` if the connection closed first.
pub async fn read_request<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Request>> {
    let line = read_line(reader).await?;
    if line.is_empty() {
        return Ok(None);
    }
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid("the request line is malformed"));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(invalid("not an HTTP/1 request"));
    }
    let mut headers = Vec::new();
    loop {
        let line = read_line(reader).await?;
        if line.is_empty() {
            break;
        }
        if headers.len() == MAX_HEADERS {
            return Err(invalid("too many headers"));
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key.to_owned(), value.to_owned())
        })
        .collect();
    Ok(Some(Request {
        method: method.to_owned(),
        path: path.to_owned(),
        query,
        headers,
    }))
}

/// Reads one line without its line ending; an empty string at the end of the connection.
async fn read_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<String> {
    let mut line = Vec::new();
    (&mut *reader)
        .take(MAX_LINE)
        .read_until(b'\n', &mut line)
        .await?;
    if line.is_empty() {
        return Ok(String::new());
    }
    if !line.ends_with(b"\n") {
        return Err(invalid(
            "a line is too long, or the connection ended mid-line",
        ));
    }
    let line = String::from_utf8(line).map_err(|_| invalid("a line isn't UTF-8"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Bytes,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type", content_type.to_owned())],
            body: body.into(),
        }
    }

    pub fn json(status: u16, value: &impl serde::Serialize) -> Self {
        let body = serde_json::to_vec_pretty(value).expect("serializing a response");
        Self::new(status, "application/json", body)
    }

    /// An RFC 9457 problem document. `code` is what a client branches on; `detail` is prose.
    pub fn problem(status: u16, code: &str, title: &str, detail: &str) -> Self {
        let body = serde_json::json!({
            "type": "about:blank",
            "status": status,
            "code": code,
            "title": title,
            "detail": detail,
            "retryable": status == 503,
        });
        let mut response = Self::json(status, &body);
        response.headers[0].1 = "application/problem+json".into();
        response
    }

    pub fn not_modified() -> Self {
        Self {
            status: 304,
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }

    pub fn header(mut self, name: &'static str, value: String) -> Self {
        self.headers.push((name, value));
        self
    }

    /// The bytes to send. A HEAD request gets the headers alone, Content-Length included.
    pub fn render(self, head_only: bool) -> Vec<u8> {
        let reason = match self.status {
            200 => "OK",
            304 => "Not Modified",
            400 => "Bad Request",
            404 => "Not Found",
            405 => "Method Not Allowed",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Error",
        };
        let mut head = format!("HTTP/1.1 {} {reason}\r\n", self.status);
        for (name, value) in &self.headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str(&format!(
            "Content-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
            self.body.len()
        ));
        let mut out = head.into_bytes();
        if !head_only {
            out.extend_from_slice(&self.body);
        }
        out
    }
}

/// `Sun, 22 Nov 2026 19:10:01 GMT`, the date format of HTTP headers.
pub fn http_date(time: SystemTime) -> String {
    let (year, month, day, hour, minute, second, weekday) = civil(time);
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    format!(
        "{}, {day:02} {} {year} {hour:02}:{minute:02}:{second:02} GMT",
        DAYS[weekday],
        MONTHS[month as usize - 1]
    )
}

/// `2026-11-22T19:10:01Z`.
pub fn rfc3339(time: SystemTime) -> String {
    let (year, month, day, hour, minute, second, _) = civil(time);
    format!("{year}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// The UTC date and time of `time`, and its weekday counted from Thursday (1970-01-01 was one).
fn civil(time: SystemTime) -> (i64, u32, u32, u64, u64, u64, usize) {
    let seconds = time.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let days = (seconds / 86_400) as i64;
    let of_day = seconds % 86_400;
    // Howard Hinnant's days-to-civil algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (
        year,
        month,
        day,
        of_day / 3600,
        of_day / 60 % 60,
        of_day % 60,
        days.rem_euclid(7) as usize,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    #[test]
    fn formats_dates() {
        let time = UNIX_EPOCH + Duration::from_secs(1_795_374_601);
        assert_eq!(http_date(time), "Sun, 22 Nov 2026 19:10:01 GMT");
        assert_eq!(rfc3339(time), "2026-11-22T19:10:01Z");
        assert_eq!(http_date(UNIX_EPOCH), "Thu, 01 Jan 1970 00:00:00 GMT");
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_164_800);
        assert_eq!(rfc3339(leap), "2024-02-29T00:00:00Z");
    }

    #[tokio::test]
    async fn reads_a_request_head() {
        let raw = b"GET /v1/cameras/cam0/still?max_age=60&wait=5 HTTP/1.1\r\nHost: pi:8555\r\n\
                    If-None-Match: \"7\"\r\n\r\n";
        let mut reader = &raw[..];
        let request = read_request(&mut reader).await.unwrap().unwrap();
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/v1/cameras/cam0/still");
        assert_eq!(request.query("max_age"), Some("60"));
        assert_eq!(request.query("wait"), Some("5"));
        assert_eq!(request.header("if-none-match"), Some("\"7\""));
        let mut empty = &b""[..];
        assert!(read_request(&mut empty).await.unwrap().is_none());
        let mut garbage = &b"GARBAGE\r\n\r\n"[..];
        assert!(read_request(&mut garbage).await.is_err());
    }
}
