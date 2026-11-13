use alloc::vec::Vec;

use crate::Error;
use crate::rtmp_timestamp::{RtmpTimestamp, RtmpTimestampDelta};

/// メディアフレーム（音声または映像）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaFrame {
    /// 音声フレーム
    Audio(AudioFrame),

    /// 映像フレーム
    Video(VideoFrame),
}

/// エンコードされた音声データを含む音声フレーム
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFrame {
    /// このフレームのタイムスタンプ（ミリ秒単位）
    pub timestamp: RtmpTimestamp,

    /// 音声フォーマット（MP3、AAC など）
    pub format: AudioFormat,

    /// サンプリングレート（5.5kHz、11kHz、22kHz、44kHz のいずれか）
    pub sample_rate: AudioSampleRate,

    /// サンプルが 8 ビット（true）か 16 ビット（false）か
    pub is_8bit_sample: bool,

    /// 音声がステレオ（true）かモノラル（false）か
    pub is_stereo: bool,

    /// ペイロードデータが AAC シーケンスヘッダーであるかどうか
    pub is_aac_sequence_header: bool,

    /// エンコードされた音声データペイロード
    pub data: Vec<u8>,
}

impl AudioFrame {
    /// AAC 用のサンプルレート
    ///
    /// FLV の仕様で「AAC の場合は固定値を使用する」と規定されている
    /// （この値は無視されて、デコーダーはビットストリームから適切な値を取得する）
    pub const AAC_SAMPLE_RATE: AudioSampleRate = AudioSampleRate::Khz44;

    /// AAC 用のステレオフラグの値
    ///
    /// FLV の仕様で「AAC の場合は固定値を使用する」と規定されている
    /// （この値は無視されて、デコーダーはビットストリームから適切な値を取得する）
    pub const AAC_STEREO: bool = true;
}

/// エンコードされた映像データを含む映像フレーム
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    /// このフレームのタイムスタンプ（ミリ秒単位）
    pub timestamp: RtmpTimestamp,

    /// H.264/AVC の合成時間オフセット（デコード時刻と表示時刻の差分）
    pub composition_timestamp_offset: RtmpTimestampDelta,

    /// このフレームのタイプ（キーフレーム、インターフレーム など）
    pub frame_type: VideoFrameType,

    /// 使用されている映像コーデック（H.264、VP6 など）
    pub codec: VideoCodec,

    /// AVC パケットタイプ（H.264 の場合にペイロードデータが何を含むかを示す）
    pub avc_packet_type: Option<AvcPacketType>,

    /// エンコードされた映像データペイロード
    pub data: Vec<u8>,
}

impl VideoFrame {
    /// このフレームがキーフレームであるかどうかを返す
    pub fn is_keyframe(&self) -> bool {
        self.frame_type == VideoFrameType::KeyFrame
    }
}

/// AVC（H.264）パケットのタイプ
///
/// AVC 映像パケット内に含まれるデータの種類を示す
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AvcPacketType {
    /// デコーダー設定データを含む AVC シーケンスヘッダー
    SequenceHeader = 0,

    /// 映像データを含む 1 つ以上の NAL ユニット
    NalUnit = 1,

    /// AVC シーケンス終了マーカー
    EndOfSequence = 2,
}

/// 映像フレームのタイプ
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VideoFrameType {
    /// 独立してデコードできるキーフレーム（イントラフレーム）
    KeyFrame = 1,

    /// 前のフレームに依存するインターフレーム
    InterFrame = 2,

    /// 再生に影響を与えずにスキップできる破棄可能インターフレーム
    DisposableInterFrame = 3,

    /// 生成されたキーフレーム（サーバー使用予約）
    GeneratedKeyFrame = 4,

    /// 映像情報またはコマンドフレーム
    VideoInfoOrCommandFrame = 5,
}

/// エンコーディングに使用される映像コーデック
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum VideoCodec {
    /// JPEG 画像コーデック
    Jpeg = 1,

    /// Sorenson H.263 コーデック
    H263 = 2,

    /// スクリーン映像コーデック
    ScreenVideo = 3,

    /// On2 VP6 コーデック
    Vp6 = 4,

    /// アルファチャネル付き On2 VP6 コーデック
    Vp6WithAlpha = 5,

    /// スクリーン映像バージョン 2 コーデック
    ScreenVideoV2 = 6,

    /// H.264/AVC コーデック
    Avc = 7,
}

/// エンコーディングに使用される音声フォーマット（コーデック）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AudioFormat {
    /// ADPCM コーデック
    Adpcm = 1,

    /// MP3 コーデック
    Mp3 = 2,

    /// リトルエンディアン リニア PCM コーデック
    LinearPcmLittleEndian = 3,

    /// Nellymoser コーデック（16kHz、モノラル）
    Nellymoser16khzMono = 4,

    /// Nellymoser コーデック（8kHz、モノラル）
    Nellymoser8KhzMono = 5,

    /// 様々なサンプリングレートの Nellymoser コーデック
    Nellymoser = 6,

    /// G.711 A-law 対数 PCM コーデック
    G711AlawLogarithmicPcm = 7,

    /// G.711 mu-law 対数 PCM コーデック
    G711MuLawLogarithmicPcm = 8,

    /// AAC コーデック
    Aac = 10,

    /// Speex コーデック
    Speex = 11,

    /// 8kHz の MP3 コーデック
    Mp3_8khz = 14,

    /// デバイス固有サウンドコーデック
    DeviceSpecificSound = 15,
}

/// 音声サンプリングレート
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AudioSampleRate {
    /// 5.5kHz サンプリングレート
    Khz5 = 0,

    /// 11kHz サンプリングレート
    Khz11 = 1,

    /// 22kHz サンプリングレート
    Khz22 = 2,

    /// 44kHz サンプリングレート
    Khz44 = 3,
}

/// AVC（H.264）デコーダ設定データ（AVCDecoderConfigurationRecord）を表す構造体
///
/// FLV の AVC シーケンスヘッダー内に含まれるデコーダ設定情報を表現します。
/// H.264 ビデオストリームをデコードするために必要な SPS（Sequence Parameter Set）
/// と PPS（Picture Parameter Set）を含みます。
///
/// # バイナリフォーマット
///
/// ```text
/// 0          1          2          3          4
/// +----------+----------+----------+----------+----------+
/// | Version  | Profile  | Profile  | Level    | Length   |
/// |          | Ind.     | Compat.  | Ind.     | Size     |
/// +----------+----------+----------+----------+----------+
///                                             |<-3 bits->|
///
/// 5          6          7          ...
/// +----------+----------+----------+
/// | SPS Cnt  | SPS Length (2 bytes) | SPS Data ...
/// |<-5 bits->|
/// +----------+
/// ...
/// +----------+----------+----------+
/// | PPS Cnt  | PPS Length (2 bytes) | PPS Data ...
/// +----------+
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvcSequenceHeader {
    /// AVC プロファイルインジケーション
    pub avc_profile_indication: u8,

    /// プロファイル互換性フラグ
    pub profile_compatibility: u8,

    /// AVC レベルインジケーション
    pub avc_level_indication: u8,

    /// NAL ユニット長フィールドのサイズ - 1（通常は 3、つまり 4 バイト）
    /// 値域: 0-3
    pub length_size_minus_one: u8,

    /// Sequence Parameter Set（SPS）リスト
    /// 最大31個（5ビット）
    pub sps_list: Vec<Vec<u8>>,

    /// Picture Parameter Set（PPS）リスト
    /// 最大255個（8ビット）
    pub pps_list: Vec<Vec<u8>>,
}

impl AvcSequenceHeader {
    /// AVC デコーダ設定データのバージョン（固定値）
    const CONFIGURATION_VERSION: u8 = 1;

    /// SPS リストの最大数（FLV仕様）
    const MAX_SPS_COUNT: usize = 31;

    /// PPS リストの最大数（FLV仕様）
    const MAX_PPS_COUNT: usize = 255;

    /// SPS の最大サイズ（バイト単位）
    const MAX_SPS_SIZE: usize = 4096;

    /// PPS の最大サイズ（バイト単位）
    const MAX_PPS_SIZE: usize = 4096;

    /// バイト列をパースして [`AvcSequenceHeader`] インスタンスを生成する
    ///
    /// 通常はこのバイト列は [`VideoFrame::avc_packet_type`] が [`AvcPacketType::SequenceHeader`] の場合に
    /// [`VideoFrame::data`] に格納されている値となる
    ///
    /// # エラー
    ///
    /// - データが短すぎる場合
    /// - サポートされていないバージョン（1以外）の場合
    /// - SPS/PPS データが不完全な場合
    /// - SPS/PPS が空の場合
    /// - SPS/PPS のサイズが上限を超える場合
    pub fn from_bytes(data: &[u8]) -> Result<Self, Error> {
        // 最小バイト数: configVersion(1) + profile(1) + compat(1) + level(1) +
        //              length_size(1) + numSPS(1) + numPPS(1) = 7 bytes
        if data.len() < 7 {
            return Err(Error::invalid_data(
                "AVCDecoderConfigurationRecord too short (expected at least 7 bytes)",
            ));
        }

        let configuration_version = data[0];
        if configuration_version != Self::CONFIGURATION_VERSION {
            return Err(Error::unsupported(format!(
                "unsupported AVC configuration version: {} (expected {})",
                configuration_version,
                Self::CONFIGURATION_VERSION
            )));
        }

        let avc_profile_indication = data[1];
        let profile_compatibility = data[2];
        let avc_level_indication = data[3];

        // length_size_minus_one は下位 2 ビットのみ使用
        let length_size_minus_one = data[4] & 0x03;

        let mut offset = 5;
        let mut sps_list = Vec::new();
        let mut pps_list = Vec::new();

        // SPS ユニット群をパース
        if offset >= data.len() {
            return Err(Error::invalid_data("incomplete SPS count field (offset 5)"));
        }

        let num_sps = (data[offset] & 0x1F) as usize;
        if num_sps > Self::MAX_SPS_COUNT {
            return Err(Error::invalid_data(format!(
                "SPS count exceeds maximum ({} > {})",
                num_sps,
                Self::MAX_SPS_COUNT
            )));
        }
        offset += 1;

        if num_sps == 0 {
            return Err(Error::invalid_data("SPS list must not be empty"));
        }

        for i in 0..num_sps {
            if offset + 2 > data.len() {
                return Err(Error::invalid_data(format!(
                    "incomplete SPS length field at index {}: need 2 bytes, have {}",
                    i,
                    data.len() - offset
                )));
            }

            let sps_length = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;

            if sps_length > Self::MAX_SPS_SIZE {
                return Err(Error::invalid_data(format!(
                    "SPS size exceeds maximum at index {}: {} > {}",
                    i,
                    sps_length,
                    Self::MAX_SPS_SIZE
                )));
            }

            if offset + sps_length > data.len() {
                return Err(Error::invalid_data(format!(
                    "incomplete SPS data at index {}: need {} bytes, have {}",
                    i,
                    sps_length,
                    data.len() - offset
                )));
            }

            sps_list.push(data[offset..offset + sps_length].to_vec());
            offset += sps_length;
        }

        // PPS ユニット群をパース
        if offset >= data.len() {
            return Err(Error::invalid_data("incomplete PPS count field"));
        }

        let num_pps = data[offset] as usize;
        if num_pps > Self::MAX_PPS_COUNT {
            return Err(Error::invalid_data(format!(
                "PPS count exceeds maximum ({} > {})",
                num_pps,
                Self::MAX_PPS_COUNT
            )));
        }
        offset += 1;

        if num_pps == 0 {
            return Err(Error::invalid_data("PPS list must not be empty"));
        }

        for i in 0..num_pps {
            if offset + 2 > data.len() {
                return Err(Error::invalid_data(format!(
                    "incomplete PPS length field at index {}: need 2 bytes, have {}",
                    i,
                    data.len() - offset
                )));
            }

            let pps_length = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;

            if pps_length > Self::MAX_PPS_SIZE {
                return Err(Error::invalid_data(format!(
                    "PPS size exceeds maximum at index {}: {} > {}",
                    i,
                    pps_length,
                    Self::MAX_PPS_SIZE
                )));
            }

            if offset + pps_length > data.len() {
                return Err(Error::invalid_data(format!(
                    "incomplete PPS data at index {}: need {} bytes, have {}",
                    i,
                    pps_length,
                    data.len() - offset
                )));
            }

            pps_list.push(data[offset..offset + pps_length].to_vec());
            offset += pps_length;
        }

        Ok(Self {
            avc_profile_indication,
            profile_compatibility,
            avc_level_indication,
            length_size_minus_one,
            sps_list,
            pps_list,
        })
    }

    /// [`AvcSequenceHeader`] インスタンスを対応するバイト列に変換する
    ///
    /// # エラー
    ///
    /// - SPS が 31 個を超える場合
    /// - PPS が 255 個を超える場合
    /// - SPS リストが空の場合
    /// - PPS リストが空の場合
    /// - SPS/PPS のサイズが上限を超える場合
    pub fn to_bytes(&self) -> Result<Vec<u8>, Error> {
        // SPS/PPS 数の検証
        if self.sps_list.is_empty() {
            return Err(Error::invalid_data("SPS list must not be empty"));
        }

        if self.pps_list.is_empty() {
            return Err(Error::invalid_data("PPS list must not be empty"));
        }

        if self.sps_list.len() > Self::MAX_SPS_COUNT {
            return Err(Error::invalid_data(format!(
                "too many SPS entries: {} (max {})",
                self.sps_list.len(),
                Self::MAX_SPS_COUNT
            )));
        }

        if self.pps_list.len() > Self::MAX_PPS_COUNT {
            return Err(Error::invalid_data(format!(
                "too many PPS entries: {} (max {})",
                self.pps_list.len(),
                Self::MAX_PPS_COUNT
            )));
        }

        // SPS/PPS のサイズ検証
        for (i, sps) in self.sps_list.iter().enumerate() {
            if sps.len() > Self::MAX_SPS_SIZE {
                return Err(Error::invalid_data(format!(
                    "SPS size exceeds maximum at index {}: {} > {}",
                    i,
                    sps.len(),
                    Self::MAX_SPS_SIZE
                )));
            }
        }

        for (i, pps) in self.pps_list.iter().enumerate() {
            if pps.len() > Self::MAX_PPS_SIZE {
                return Err(Error::invalid_data(format!(
                    "PPS size exceeds maximum at index {}: {} > {}",
                    i,
                    pps.len(),
                    Self::MAX_PPS_SIZE
                )));
            }
        }

        let mut result = vec![
            Self::CONFIGURATION_VERSION,
            self.avc_profile_indication,
            self.profile_compatibility,
            self.avc_level_indication,
            // length_size_minus_one: 下位 2 ビット、上位 6 ビットは 1 で埋める
            0xFC | (self.length_size_minus_one & 0x03),
        ];

        // SPS 数と SPS リスト
        // SPS 数: 5 ビット、上位 3 ビットは 1 で埋める
        result.push(0xE0 | (self.sps_list.len() as u8));
        for sps in &self.sps_list {
            result.extend_from_slice(&(sps.len() as u16).to_be_bytes());
            result.extend_from_slice(sps);
        }

        // PPS 数と PPS リスト（8 ビット全て使用）
        result.push(self.pps_list.len() as u8);
        for pps in &self.pps_list {
            result.extend_from_slice(&(pps.len() as u16).to_be_bytes());
            result.extend_from_slice(pps);
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_avc_sequence_header_roundtrip() {
        let header = AvcSequenceHeader {
            avc_profile_indication: 0x42,
            profile_compatibility: 0xC0,
            avc_level_indication: 0x1F,
            length_size_minus_one: 3,
            sps_list: vec![vec![0x01, 0x02, 0x03]],
            pps_list: vec![vec![0x04, 0x05]],
        };

        let bytes = header.to_bytes().expect("to_bytes failed");
        let parsed = AvcSequenceHeader::from_bytes(&bytes).expect("from_bytes failed");

        assert_eq!(header, parsed);
    }

    #[test]
    fn test_avc_sequence_header_too_short() {
        let short_data = vec![0x01, 0x02, 0x03];
        assert!(AvcSequenceHeader::from_bytes(&short_data).is_err());
    }

    #[test]
    fn test_avc_sequence_header_invalid_version() {
        let data = vec![0x02, 0x42, 0xC0, 0x1F, 0xFC, 0xE0, 0x00];
        let result = AvcSequenceHeader::from_bytes(&data);
        assert!(result.is_err());
    }

    #[test]
    fn test_avc_sequence_header_multiple_sps_pps() {
        let header = AvcSequenceHeader {
            avc_profile_indication: 0x42,
            profile_compatibility: 0xC0,
            avc_level_indication: 0x1F,
            length_size_minus_one: 3,
            sps_list: vec![vec![1, 2, 3], vec![4, 5, 6]],
            pps_list: vec![vec![7, 8], vec![9, 10]],
        };

        let bytes = header.to_bytes().expect("to_bytes failed");
        let parsed = AvcSequenceHeader::from_bytes(&bytes).expect("from_bytes failed");

        assert_eq!(header, parsed);
    }

    #[test]
    fn test_avc_sequence_header_empty_sps() {
        let header = AvcSequenceHeader {
            avc_profile_indication: 0x42,
            profile_compatibility: 0xC0,
            avc_level_indication: 0x1F,
            length_size_minus_one: 3,
            sps_list: vec![],
            pps_list: vec![vec![0x04, 0x05]],
        };

        assert!(header.to_bytes().is_err());
    }

    #[test]
    fn test_avc_sequence_header_empty_pps() {
        let header = AvcSequenceHeader {
            avc_profile_indication: 0x42,
            profile_compatibility: 0xC0,
            avc_level_indication: 0x1F,
            length_size_minus_one: 3,
            sps_list: vec![vec![0x01, 0x02, 0x03]],
            pps_list: vec![],
        };

        assert!(header.to_bytes().is_err());
    }

    #[test]
    fn test_avc_sequence_header_sps_too_large() {
        let header = AvcSequenceHeader {
            avc_profile_indication: 0x42,
            profile_compatibility: 0xC0,
            avc_level_indication: 0x1F,
            length_size_minus_one: 3,
            sps_list: vec![vec![0; 5000]], // MAX_SPS_SIZE = 4096
            pps_list: vec![vec![0x04, 0x05]],
        };

        assert!(header.to_bytes().is_err());
    }

    #[test]
    fn test_avc_sequence_header_pps_too_large() {
        let header = AvcSequenceHeader {
            avc_profile_indication: 0x42,
            profile_compatibility: 0xC0,
            avc_level_indication: 0x1F,
            length_size_minus_one: 3,
            sps_list: vec![vec![0x01, 0x02, 0x03]],
            pps_list: vec![vec![0; 5000]], // MAX_PPS_SIZE = 4096
        };

        assert!(header.to_bytes().is_err());
    }

    #[test]
    fn test_avc_sequence_header_empty_sps_from_bytes() {
        // num_sps = 0 のデータ
        let data = vec![0x01, 0x42, 0xC0, 0x1F, 0xFC, 0xE0, 0x00];
        assert!(AvcSequenceHeader::from_bytes(&data).is_err());
    }

    #[test]
    fn test_avc_sequence_header_empty_pps_from_bytes() {
        // 最小限の有効なSPSとnum_pps = 0
        let mut data = vec![0x01, 0x42, 0xC0, 0x1F, 0xFC, 0xE1]; // num_sps = 1
        data.extend_from_slice(&[0x00, 0x01]); // sps_length = 1
        data.extend_from_slice(&[0xFF]); // sps_data
        data.push(0x00); // num_pps = 0

        assert!(AvcSequenceHeader::from_bytes(&data).is_err());
    }

    #[test]
    fn test_avc_sequence_header_sps_too_large_from_bytes() {
        // MAX_SPS_SIZE を超えるサイズを指定
        let mut data = vec![0x01, 0x42, 0xC0, 0x1F, 0xFC, 0xE1]; // num_sps = 1
        data.extend_from_slice(&[0x10, 0x01]); // sps_length = 4097 (MAX_SPS_SIZE より大きい)

        assert!(AvcSequenceHeader::from_bytes(&data).is_err());
    }

    #[test]
    fn test_avc_sequence_header_pps_too_large_from_bytes() {
        // MAX_PPS_SIZE を超えるサイズを指定
        let mut data = vec![0x01, 0x42, 0xC0, 0x1F, 0xFC, 0xE1]; // num_sps = 1
        data.extend_from_slice(&[0x00, 0x01]); // sps_length = 1
        data.extend_from_slice(&[0xFF]); // sps_data
        data.extend_from_slice(&[0x01]); // num_pps = 1
        data.extend_from_slice(&[0x10, 0x01]); // pps_length = 4097 (MAX_PPS_SIZE より大きい)

        assert!(AvcSequenceHeader::from_bytes(&data).is_err());
    }
}
