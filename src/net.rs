use binread::{BinRead, BinReaderExt};
use byteorder::{LittleEndian, WriteBytesExt};
use bytes::BytesMut;
use protobuf::ProtobufEnum;
use std::convert::{TryFrom, TryInto};
use std::io::{Cursor, Write};
use std::mem::size_of;
use steam_vent_crypto::generate_session_key;
use steam_vent_proto::enums_clientserver::EMsg;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpStream, ToSocketAddrs};

#[derive(Debug, Error)]
pub enum NetworkError {
    #[error("{0}")]
    IO(#[from] std::io::Error),
    #[error("Invalid message header")]
    InvalidHeader,
    #[error("Invalid message kind {0}")]
    InvalidMessageKind(i32),
    #[error("Unexpected handshake {0}")]
    UnexpectedHandshake(&'static str),
}

pub type Result<T> = std::result::Result<T, NetworkError>;

pub async fn raw_connect<A: ToSocketAddrs>(addr: A) -> Result<(RawSteamReader, RawSteamWriter)> {
    let stream = TcpStream::connect(addr).await?;
    let (read, write) = stream.into_split();
    Ok((
        RawSteamReader {
            tcp: BufReader::new(read),
            buff: BytesMut::with_capacity(1024),
        },
        RawSteamWriter {
            tcp: BufWriter::new(write),
        },
    ))
}

const MAGIC: [u8; 4] = *b"VT01";

#[derive(Debug, Default, Copy, Clone, BinRead)]
#[repr(C)]
struct Header {
    length: u32,
    magic: [u8; 4],
}

impl Header {
    fn validate(&self) -> Result<()> {
        if self.magic != MAGIC {
            Err(NetworkError::InvalidHeader)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub struct RawNetMessage<'a> {
    kind: EMsg,
    is_protobuf: bool,
    data: &'a [u8],
}

impl<'a> TryFrom<&'a [u8]> for RawNetMessage<'a> {
    type Error = NetworkError;

    fn try_from(value: &'a [u8]) -> Result<Self> {
        let kind = i32::from_le_bytes(
            value[0..4]
                .try_into()
                .map_err(|_| NetworkError::InvalidMessageKind(0))?,
        );

        let is_protobuf = kind < 0;

        let kind = match steam_vent_proto::enums_clientserver::EMsg::from_i32(kind.abs()) {
            Some(kind) => kind,
            None => return Err(NetworkError::InvalidMessageKind(kind)),
        };

        Ok(RawNetMessage {
            kind,
            is_protobuf,
            data: &value[4..],
        })
    }
}

pub struct RawSteamReader {
    tcp: BufReader<OwnedReadHalf>,
    buff: BytesMut,
}

impl RawSteamReader {
    pub async fn read_buff(&mut self) -> Result<&[u8]> {
        let mut header_bytes = [0; 8];
        self.tcp.read_exact(&mut header_bytes).await?;
        let header: Header = Cursor::new(&header_bytes[..]).read_le().unwrap();
        header.validate()?;

        dbg!(header);

        self.buff.resize(header.length as usize, 0);
        self.tcp.read_exact(self.buff.as_mut()).await?;
        Ok(self.buff.as_ref())
    }

    pub async fn read<'a>(&'a mut self) -> Result<RawNetMessage<'a>> {
        self.read_buff().await.and_then(RawNetMessage::try_from)
    }
}

pub struct RawSteamWriter {
    tcp: BufWriter<OwnedWriteHalf>,
}

impl RawSteamWriter {
    pub async fn write_buff(&mut self, data: &[u8]) -> Result<()> {
        // self.tcp.write_u32(data.len() as u32).await?;
        // self.tcp.write_all(&MAGIC[..]).await?;
        // self.tcp.write_all(data).await?;

        let mut full = Vec::with_capacity(data.len() + 8);
        WriteBytesExt::write_u32::<LittleEndian>(&mut full, data.len() as u32)?;
        Write::write_all(&mut full, &MAGIC)?;
        Write::write_all(&mut full, data)?;

        self.tcp.write_all(&full).await?;

        self.tcp.flush().await?;

        Ok(())
    }
}

#[derive(Debug, BinRead)]
struct ChannelEncryptRequestBody {
    target_job_id: u64,
    source_job_id: u64,
    protocol: u32,
    universe: u32,
    nonce: [u8; 16],
}

#[derive(Debug, BinRead)]
struct ChannelEncryptResultBody {
    target_job_id: u64,
    source_job_id: u64,
    result: u32,
}

pub struct SteamReader {
    _raw: RawSteamReader,
    _key: [u8; 32],
}

pub struct SteamWriter {
    _raw: RawSteamWriter,
    _key: [u8; 32],
}

pub async fn connect<A: ToSocketAddrs>(addr: A) -> Result<(SteamReader, SteamWriter)> {
    let (mut raw_reader, mut raw_writer) = raw_connect(addr).await?;

    let encrypt_request = match raw_reader.read().await? {
        RawNetMessage {
            kind: EMsg::k_EMsgChannelEncryptRequest,
            data,
            ..
        } => data,
        _ => {
            return Err(NetworkError::UnexpectedHandshake(
                "Expected encrypt request",
            ))
        }
    };

    if encrypt_request.len() != size_of::<ChannelEncryptRequestBody>() {
        return Err(NetworkError::UnexpectedHandshake(
            "Malformed encrypt request",
        ));
    }

    let encrypt_request: ChannelEncryptRequestBody = Cursor::new(encrypt_request)
        .read_le()
        .map_err(|_| NetworkError::UnexpectedHandshake("Invalid encrypt request body"))?;

    let key = generate_session_key(Some(&encrypt_request.nonce));

    let mut response_buf = Vec::with_capacity(4 + 8 + 8 + 4 + 4 + key.encrypted.len() + 4 + 4);
    let response = ClientEncryptResponse {
        target_job_id: u64::max_value(),
        source_job_id: u64::max_value(),
        protocol: encrypt_request.protocol,
        encrypted_key: key.encrypted,
    };
    response.encode(&mut response_buf)?;

    raw_writer.write_buff(&response_buf).await?;

    let encrypt_response = match raw_reader.read().await? {
        RawNetMessage {
            kind: EMsg::k_EMsgChannelEncryptResult,
            data,
            ..
        } => data,
        _ => return Err(NetworkError::UnexpectedHandshake("Expected encrypt result")),
    };

    let encrypt_response: ChannelEncryptResultBody = Cursor::new(encrypt_response)
        .read_le()
        .map_err(|_| NetworkError::UnexpectedHandshake("Invalid encrypt result body"))?;

    dbg!(&encrypt_response);

    panic!()
}

struct ClientEncryptResponse {
    target_job_id: u64,
    source_job_id: u64,
    protocol: u32,
    encrypted_key: Vec<u8>,
}

impl ClientEncryptResponse {
    fn encode<W: Write>(&self, mut writer: W) -> std::io::Result<()> {
        writer.write_i32::<LittleEndian>(EMsg::k_EMsgChannelEncryptResponse.value())?;
        writer.write_u64::<LittleEndian>(self.target_job_id)?;
        writer.write_u64::<LittleEndian>(self.source_job_id)?;
        writer.write_u32::<LittleEndian>(self.protocol)?;
        writer.write_u32::<LittleEndian>(self.encrypted_key.len() as u32)?;
        writer.write_all(&self.encrypted_key)?;
        writer.write_u32::<LittleEndian>(crc::crc32::checksum_ieee(&self.encrypted_key))?;
        writer.write_u32::<LittleEndian>(0)?;

        Ok(())
    }
}
