use alloc::borrow::ToOwned;
use alloc::string::String;
use core::fmt;
use core::str::FromStr;

use crate::Error;

/// RTMP 用の URL
///
/// # NOTE
///
/// [`core::str::FromStr`] の実装では [`RtmpUrl::parse()`] が使用されます。
/// もしストリーム名を URL 文字列とは別に指定したい場合には [`RtmpUrl::parse_with_stream_name()`] を使用してください。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RtmpUrl {
    /// RTMP サーバーのホスト名または IP アドレス
    pub host: String,

    /// RTMP サーバーのポート番号
    pub port: u16,

    /// RTMP アプリケーション名
    pub app: String,

    /// ストリーム名
    pub stream_name: String,

    /// TLS 接続を使用するかどうか (rtmps の場合 true)
    pub tls: bool,
}

impl RtmpUrl {
    /// ストリーム名を含む RTMP URL をパースします: `rtmp[s]://host[:port]/app/stream_name`
    ///
    /// パス部分に複数の `/` が含まれる場合、最後の `/` でアプリケーション名とストリーム名に分割されます
    ///
    /// ポートが省略された場合、デフォルトポートが使用されます:
    /// - rtmp: 1935
    /// - rtmps: 443
    pub fn parse(s: &str) -> Result<Self, Error> {
        let (tls, host, port, path) = Self::parse_scheme_and_host_port(s)?;

        let (app, stream_name) = path
            .rsplit_once('/')
            .ok_or_else(|| Error::invalid_input("missing app and/or stream_name in path"))?;
        if app.is_empty() {
            return Err(Error::invalid_input("app name cannot be empty"));
        }
        if stream_name.is_empty() {
            return Err(Error::invalid_input("stream name cannot be empty"));
        }

        Ok(RtmpUrl {
            host: host.to_owned(),
            port,
            app: app.to_owned(),
            stream_name: stream_name.to_owned(),
            tls,
        })
    }

    /// ストリーム名を別途指定して RTMP URL をパースします: `rtmp[s]://host[:port]/app`
    ///
    /// ポートが省略された場合、デフォルトポートが使用されます:
    /// - rtmp: 1935
    /// - rtmps: 443
    pub fn parse_with_stream_name(s: &str, stream_name: &str) -> Result<Self, Error> {
        let (tls, host, port, app) = Self::parse_scheme_and_host_port(s)?;

        if app.is_empty() {
            return Err(Error::invalid_input("app name cannot be empty"));
        }
        if stream_name.is_empty() {
            return Err(Error::invalid_input("stream name cannot be empty"));
        }

        Ok(RtmpUrl {
            host: host.to_owned(),
            port,
            app: app.to_owned(),
            stream_name: stream_name.to_owned(),
            tls,
        })
    }

    fn parse_scheme_and_host_port(s: &str) -> Result<(bool, &str, u16, &str), Error> {
        // scheme://
        let (scheme, rest) = s
            .split_once("://")
            .ok_or_else(|| Error::invalid_input("missing '://' in RTMP URL"))?;
        let tls = match scheme {
            "rtmp" => false,
            "rtmps" => true,
            _ => {
                return Err(Error::invalid_input(format!(
                    "invalid scheme '{scheme}', expected 'rtmp' or 'rtmps'"
                )));
            }
        };

        // host[:port]/path
        let (host_port, path) = rest
            .split_once('/')
            .ok_or_else(|| Error::invalid_input("missing '/' separator for app name"))?;

        // host, port
        let (host, port) = parse_host_port(host_port, tls)?;

        if host.is_empty() {
            return Err(Error::invalid_input("host cannot be empty"));
        }

        Ok((tls, host, port, path))
    }
}

impl fmt::Display for RtmpUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scheme = if self.tls { "rtmps" } else { "rtmp" };
        write!(
            f,
            "{}://{}:{}/{}/{}",
            scheme, self.host, self.port, self.app, self.stream_name
        )
    }
}

impl FromStr for RtmpUrl {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn parse_host_port(host_port: &str, tls: bool) -> Result<(&str, u16), Error> {
    // ポート番号が含まれているかチェック
    // IPv6 アドレスの場合は ] の後ろにポートがある
    let (host, port_str) = if host_port.starts_with('[') {
        // IPv6 アドレスの場合: [::1]:1935
        if let Some(bracket_end) = host_port.find(']') {
            let host = &host_port[..=bracket_end];
            let remainder = &host_port[bracket_end + 1..];

            if remainder.is_empty() {
                // ポート番号なし
                (host, None)
            } else if let Some(remainder) = remainder.strip_prefix(':') {
                // ポート番号あり
                (host, Some(remainder))
            } else {
                return Err(Error::invalid_input(
                    "invalid format after IPv6 address, expected ':' before port",
                ));
            }
        } else {
            return Err(Error::invalid_input(
                "invalid IPv6 address format, missing ']'",
            ));
        }
    } else {
        // IPv4 またはホスト名
        if let Some(colon_pos) = host_port.rfind(':') {
            // ':' が見つかった場合、ポート番号がある可能性
            let potential_port = &host_port[colon_pos + 1..];
            // ポート部分が数字のみか確認（IPv6でない場合）
            if potential_port.chars().all(|c| c.is_ascii_digit()) {
                (&host_port[..colon_pos], Some(potential_port))
            } else {
                // IPv6 で '[' がない不正な形式
                return Err(Error::invalid_input("invalid host:port format"));
            }
        } else {
            // ':' がない（ポート番号なし）
            (host_port, None)
        }
    };

    // ポート番号をパース
    let port = match port_str {
        Some(port_s) => port_s
            .parse::<u16>()
            .map_err(|e| Error::invalid_input(format!("invalid port number '{port_s}': {e}")))?,
        None => {
            if tls {
                443
            } else {
                1935
            }
        }
    };

    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::string::ToString;
    use core::str::FromStr;

    #[test]
    fn test_basic_rtmp_url() {
        let url = RtmpUrl::from_str("rtmp://example.com:1935/live/stream").unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 1935);
        assert_eq!(url.app, "live");
        assert_eq!(url.stream_name, "stream");
        assert!(!url.tls);
    }

    #[test]
    fn test_rtmps_url() {
        let url = RtmpUrl::from_str("rtmps://example.com:443/live/stream").unwrap();
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 443);
        assert_eq!(url.app, "live");
        assert_eq!(url.stream_name, "stream");
        assert!(url.tls);
    }

    #[test]
    fn test_default_port_rtmp() {
        let url = RtmpUrl::from_str("rtmp://example.com/live/stream").unwrap();
        assert_eq!(url.port, 1935);
    }

    #[test]
    fn test_default_port_rtmps() {
        let url = RtmpUrl::from_str("rtmps://example.com/live/stream").unwrap();
        assert_eq!(url.port, 443);
    }

    #[test]
    fn test_nested_app_path() {
        let url = RtmpUrl::from_str("rtmp://example.com/app/path/stream").unwrap();
        assert_eq!(url.app, "app/path");
        assert_eq!(url.stream_name, "stream");
    }

    #[test]
    fn test_display() {
        let url = RtmpUrl {
            host: "example.com".to_owned(),
            port: 1935,
            app: "live".to_owned(),
            stream_name: "stream".to_owned(),
            tls: false,
        };
        assert_eq!(url.to_string(), "rtmp://example.com:1935/live/stream");
    }

    #[test]
    fn test_display_rtmps() {
        let url = RtmpUrl {
            host: "example.com".to_owned(),
            port: 443,
            app: "live".to_owned(),
            stream_name: "stream".to_owned(),
            tls: true,
        };
        assert_eq!(url.to_string(), "rtmps://example.com:443/live/stream");
    }

    #[test]
    fn test_round_trip() {
        let original = "rtmp://example.com:1935/live/stream";
        let url = RtmpUrl::from_str(original).unwrap();
        assert_eq!(url.to_string(), original);
    }

    #[test]
    fn test_invalid_scheme() {
        let result = RtmpUrl::from_str("http://example.com/live/stream");
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_scheme_separator() {
        let result = RtmpUrl::from_str("rtmp example.com/live/stream");
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_path_separator() {
        let result = RtmpUrl::from_str("rtmp://example.com");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_host() {
        let result = RtmpUrl::from_str("rtmp://:1935/live/stream");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_app() {
        let result = RtmpUrl::from_str("rtmp://example.com//stream");
        assert!(result.is_err());
    }

    #[test]
    fn test_empty_stream_name() {
        let result = RtmpUrl::from_str("rtmp://example.com/live/");
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_port() {
        let result = RtmpUrl::from_str("rtmp://example.com:invalid/live/stream");
        assert!(result.is_err());
    }

    #[test]
    fn test_missing_stream_name() {
        let result = RtmpUrl::from_str("rtmp://example.com/live");
        assert!(result.is_err());
    }

    #[test]
    fn test_ipv4_address() {
        let url = RtmpUrl::from_str("rtmp://192.168.1.1:1935/live/stream").unwrap();
        assert_eq!(url.host, "192.168.1.1");
    }

    #[test]
    fn test_clone_and_equality() {
        let url1 = RtmpUrl::from_str("rtmp://example.com/live/stream").unwrap();
        let url2 = url1.clone();
        assert_eq!(url1, url2);
    }

    // IPv6 対応テストケース
    #[test]
    fn test_ipv6_address_with_port() {
        let url = RtmpUrl::from_str("rtmp://[::1]:1935/live/stream").unwrap();
        assert_eq!(url.host, "[::1]");
        assert_eq!(url.port, 1935);
        assert_eq!(url.app, "live");
        assert_eq!(url.stream_name, "stream");
    }

    #[test]
    fn test_ipv6_address_full() {
        let url = RtmpUrl::from_str("rtmp://[2001:db8::1]:1935/live/stream").unwrap();
        assert_eq!(url.host, "[2001:db8::1]");
        assert_eq!(url.port, 1935);
    }

    #[test]
    fn test_ipv6_address_default_port_rtmp() {
        let url = RtmpUrl::from_str("rtmp://[::1]/live/stream").unwrap();
        assert_eq!(url.host, "[::1]");
        assert_eq!(url.port, 1935);
    }

    #[test]
    fn test_ipv6_address_default_port_rtmps() {
        let url = RtmpUrl::from_str("rtmps://[::1]/live/stream").unwrap();
        assert_eq!(url.host, "[::1]");
        assert_eq!(url.port, 443);
    }

    #[test]
    fn test_ipv6_display() {
        let url = RtmpUrl {
            host: "[::1]".to_owned(),
            port: 1935,
            app: "live".to_owned(),
            stream_name: "stream".to_owned(),
            tls: false,
        };
        assert_eq!(url.to_string(), "rtmp://[::1]:1935/live/stream");
    }

    #[test]
    fn test_ipv6_display_rtmps() {
        let url = RtmpUrl {
            host: "[2001:db8::1]".to_owned(),
            port: 443,
            app: "app".to_owned(),
            stream_name: "stream".to_owned(),
            tls: true,
        };
        assert_eq!(url.to_string(), "rtmps://[2001:db8::1]:443/app/stream");
    }

    #[test]
    fn test_ipv6_round_trip() {
        let original = "rtmp://[::1]:1935/live/stream";
        let url = RtmpUrl::from_str(original).unwrap();
        assert_eq!(url.to_string(), original);
    }

    #[test]
    fn test_invalid_ipv6_address() {
        // IPv6 部分の検証は RtmpUrl は行わないので成功する
        let result = RtmpUrl::from_str("rtmp://[::gggg]:1935/live/stream");
        assert!(result.is_ok());
    }

    #[test]
    fn test_ipv6_missing_closing_bracket() {
        let result = RtmpUrl::from_str("rtmp://[::1:1935/live/stream");
        assert!(result.is_err());
    }
}
