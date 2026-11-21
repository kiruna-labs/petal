use core::time::Duration;

/// RTMP のタイムスタンプを表す構造体
///
/// RTMP では、タイムスタンプはミリ秒単位の符号なし 32 ビット整数として扱われる
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RtmpTimestamp(u32);

impl RtmpTimestamp {
    /// ゼロタイムスタンプ（0 ミリ秒）
    pub const ZERO: Self = Self(0);

    /// ミリ秒単位の値から [`RtmpTimestamp`] を作成する
    pub const fn from_millis(t: u32) -> Self {
        Self(t)
    }

    /// タイムスタンプをミリ秒単位の値として取得する
    pub const fn as_millis(self) -> u32 {
        self.0
    }

    /// タイムスタンプを [`Duration`] として取得する
    pub const fn as_duration(self) -> Duration {
        Duration::from_millis(self.0 as u64)
    }

    /// 別のタイムスタンプを加算する（オーバーフロー時はラップアラウンドする）
    pub fn wrapping_add(self, other: Self) -> Self {
        Self(self.0.wrapping_add(other.0))
    }

    /// 別のタイムスタンプを減算する（結果が負になる場合は `None` を返す）
    pub fn checked_sub(self, other: Self) -> Option<Self> {
        self.0.checked_sub(other.0).map(Self)
    }
}

/// RTMP のタイムスタンプの差分を表す構造体
///
/// RTMP では、タイムスタンプの差分はミリ秒単位の符号付き 32 ビット整数として扱われる
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RtmpTimestampDelta(i32);

impl RtmpTimestampDelta {
    /// ゼロ差分（0 ミリ秒）
    pub const ZERO: Self = Self(0);

    /// ミリ秒単位の値から [`RtmpTimestampDelta`] を作成する
    pub const fn from_millis(t: i32) -> Self {
        Self(t)
    }

    /// タイムスタンプ差分をミリ秒単位の値として取得する
    pub const fn as_millis(self) -> i32 {
        self.0
    }
}
