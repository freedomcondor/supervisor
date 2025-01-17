use anyhow::{Context, Result};
use bytes::{BytesMut, Bytes, BufMut, Buf};
use std::{io, collections::HashMap, sync::Arc, net::SocketAddr};
use log;
use serde::Serialize;

use tokio::{net::{TcpListener, TcpStream}, sync::{Mutex, broadcast, mpsc, oneshot}};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::codec::{Decoder, Encoder, Framed};
use futures::StreamExt;

use std::mem::size_of;

const LUA_TNIL: i8 = 0;
const LUA_TBOOLEAN: i8 = 1;
//const LUA_TLIGHTUSERDATA: i8 = 2;
const LUA_TNUMBER: i8 = 3;
const LUA_TSTRING: i8 = 4;
const LUA_TTABLE: i8 = 5;
//const LUA_TFUNCTION: i8 = 6;
const LUA_TUSERDATA: i8 = 7;
//const LUA_TTHREAD: i8 = 8;
const LUA_TUSERDATA_VECTOR2: u8 = 1;
const LUA_TUSERDATA_VECTOR3: u8 = 2;
const LUA_TUSERDATA_QUATERNION: u8 = 3;
const MAX_MANTISSA: f64 = 9223372036854775806.0;

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum LuaType {
    String(String),
    Number(f64),
    Boolean(bool),
    Vector2(f64, f64),
    Vector3(f64, f64, f64),
    Quaternion(f64, f64, f64, f64),
    Table(Vec<(LuaType, LuaType)>),
}

fn decode_lua_usertype(buf: &mut impl Buf) -> Result<LuaType> {
    if buf.has_remaining() {
        match buf.get_u8() {
            LUA_TUSERDATA_VECTOR2 => decode_lua_vector2(buf)
                .context("Could not decode vector2"),
            LUA_TUSERDATA_VECTOR3 => decode_lua_vector3(buf)
                .context("Could not decode vector3"),
            LUA_TUSERDATA_QUATERNION => decode_lua_quaternion(buf)
                .context("Could not decode quaternion"),
            _ => Err(anyhow::anyhow!("Could not decode Lua user type"))
        }
    }
    else {
        Err(anyhow::anyhow!("Could not decode Lua user type"))
    }
}

fn decode_lua_vector2(buf: &mut impl Buf) -> Result<LuaType> {
    let x = decode_lua_number(buf)
        .context("Could not decode X")?;
    let y = decode_lua_number(buf)
        .context("Could not decode Y")?;
    match (x, y) {
        (LuaType::Number(x),
         LuaType::Number(y)) => Ok(LuaType::Vector2(x, y)),
        _ => Err(anyhow::anyhow!("Either X or Y was not a Lua number"))
    }
}

fn decode_lua_vector3(buf: &mut impl Buf) -> Result<LuaType> {
    let x = decode_lua_number(buf)
        .context("Could not decode X")?;
    let y = decode_lua_number(buf)
        .context("Could not decode Y")?;
    let z = decode_lua_number(buf)
        .context("Could not decode Z")?;
    match (x, y, z) {
        (LuaType::Number(x),
         LuaType::Number(y),
         LuaType::Number(z)) => Ok(LuaType::Vector3(x, y, z)),
        _ => Err(anyhow::anyhow!("Either X, Y, or Z was not a Lua number"))
    }
}

fn decode_lua_quaternion(buf: &mut impl Buf) -> Result<LuaType> {
    let w = decode_lua_number(buf)
        .context("Could not decode W")?;
    let x = decode_lua_number(buf)
        .context("Could not decode X")?;
    let y = decode_lua_number(buf)
        .context("Could not decode Y")?;
    let z = decode_lua_number(buf)
        .context("Could not decode Z")?;
    match (w, x, y, z) {
        (LuaType::Number(w),
         LuaType::Number(x),
         LuaType::Number(y),
         LuaType::Number(z)) => Ok(LuaType::Quaternion(w, x, y, z)),
         _ => Err(anyhow::anyhow!("Either W, X, Y, or Z was not a Lua number"))
    }
}

fn decode_lua_number(buf: &mut impl Buf) -> Result<LuaType> {
    /* handle Carlo's unusual double encoding */
    if buf.remaining() > size_of::<u64>() + size_of::<u32>() {
        let mantissa = buf.get_i64();
        let exponent = buf.get_i32();
        if mantissa == 0 {
            Ok(LuaType::Number(0.0))
        }
        else {
            let significand = ((mantissa.abs() - 1i64) as f64 / MAX_MANTISSA) / 2.0 + 0.5;
            let value = significand * 2.0f64.powi(exponent);
            if mantissa < 0 {
                Ok(LuaType::Number(-value))
            }
            else {
                Ok(LuaType::Number(value))
            }
        }
    }
    else {
        Err(anyhow::anyhow!("Could not decode Lua number"))
    }
}

fn decode_lua_string(buf: &mut impl Buf) -> Result<LuaType> {
    /* extract C string */
    let mut data = Vec::new();
    while buf.has_remaining() {
        match buf.get_u8() {
            0 => break,
            byte => data.push(byte),
        }
    }
    String::from_utf8(data)
        .map_err(|_| anyhow::anyhow!("Could not decode Lua string"))
        .map(|content| LuaType::String(content))
}

fn decode_lua_boolean(buf: &mut impl Buf) -> Result<LuaType> {
    if buf.has_remaining() {
        match buf.get_i8() {
            0 => Ok(LuaType::Boolean(false)),
            _ => Ok(LuaType::Boolean(true)),
        }
    }
    else {
        Err(anyhow::anyhow!("Could not decode Lua boolean"))
    }
}

fn decode_lua_table(buf: &mut impl Buf) -> Result<LuaType> {
    let mut table = Vec::new();
    while buf.has_remaining() {
        /* parse the key */
        let key = match buf.get_i8() {
            LUA_TBOOLEAN => decode_lua_boolean(buf),
            LUA_TNUMBER => decode_lua_number(buf),
            LUA_TSTRING => decode_lua_string(buf),
            LUA_TUSERDATA => decode_lua_usertype(buf),
            LUA_TTABLE => decode_lua_table(buf),
            LUA_TNIL => break,
            _ => Err(anyhow::anyhow!("Could not decode key")),
        }?;
        if buf.has_remaining() {
            /* parse the value */
            let value = match buf.get_i8() {
                LUA_TBOOLEAN => decode_lua_boolean(buf),
                LUA_TNUMBER => decode_lua_number(buf),
                LUA_TSTRING => decode_lua_string(buf),
                LUA_TUSERDATA => decode_lua_usertype(buf),
                LUA_TTABLE => decode_lua_table(buf),
                _ => Err(anyhow::anyhow!("Could not decode value")),
            }?;
            table.push((key, value));
        }
        else {
            anyhow::bail!("Could not decode value");
        }
    }
    Ok(LuaType::Table(table))
}

fn read_lua_table_with_string_key_and_string_value(table: &LuaType, key: String) -> String {
    match table {
        LuaType::Table(table_vec) => {
            for x in table_vec{
                match &x.0 {
                    LuaType::String(key_string) => {
                        if key_string.eq(&key) {
                            match &x.1 {
                                LuaType::String(value_string) => {
                                    return String::from(value_string)
                                }
                                _ => {
                                    return String::from("nil")
                                },
                            }
                        }
                    }
                    _ => {},
                }
            }
            String::from("nil")
        }
        _ => {String::from("nil")},
    }
}

#[derive(Debug, Default)]
struct ByteArrayCodec {
    len: Option<usize>
}

impl Decoder for ByteArrayCodec {
    type Item = Bytes;
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Bytes>, io::Error> {
        loop {
            if let Some(len) = self.len {
                if buf.len() >= len {
                    self.len = None;
                    return Ok(Some(buf.split_to(len).freeze()));
                }
                else {
                    break;
                }
            }
            else {
                if buf.len() >= 4 {
                    self.len = Some(buf.get_u32() as usize);
                }
                else {
                    break;
                }
            }
        }
        Ok(None)
    }
}

impl Encoder<Bytes> for ByteArrayCodec {
    type Error = io::Error;

    fn encode(&mut self, data: Bytes, buf: &mut BytesMut) -> Result<(), io::Error> {
        buf.reserve(data.len() + size_of::<u32>());
        buf.put_u32(data.len() as u32);
        buf.put(data);
        Ok(())
    }
}

type Peers = Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<Bytes>>>>;
type NameAddressIndex = Arc<Mutex<HashMap<String, SocketAddr>>>;

async fn client_handler(stream: TcpStream,
                        addr: SocketAddr,
                        peers: Peers,
                        name_address_index: NameAddressIndex,
                        updates_tx: broadcast::Sender<(SocketAddr, LuaType)>) {
    log::info!("{} connected to message router", addr);
    /* set up a channel for communicating with other robot sockets */
    let (tx, rx) = mpsc::channel::<Bytes>(32);
    /* wrap up socket in our ByteArrayCodec */
    let (sink, mut stream) = Framed::new(stream, ByteArrayCodec::default()).split();
    {
        peers.lock().await.insert(addr, tx);
    }
    /* send and receive messages concurrently */
    let mut forward = tokio::spawn(ReceiverStream::new(rx).map(Result::Ok).forward(sink));
    loop {
        tokio::select! {
            Some(message) = stream.next() => match message {
                Ok(mut message) => {
                    let message_clone = message.clone();

                    let mut from_s = String::from("nil");
                    let mut to_s = String::from("nil");
                    if let Ok(decoded) = decode_lua_table(&mut message) {
                        from_s = read_lua_table_with_string_key_and_string_value(&decoded, String::from("fromS"));
                        to_s = read_lua_table_with_string_key_and_string_value(&decoded, String::from("toS"));
                        let _ = updates_tx.send((addr, decoded));
                    }

                    // Add from_s to name_address_index
                    if !from_s.eq(&String::from("nil")) {
                        name_address_index.lock().await.entry(from_s.clone()).or_insert(addr);
                    }

                    // check if to_s is in address index, send only to to_s, otherwise send to everyone
                    let mut flag = false;

                    // try to send
                    if name_address_index.lock().await.contains_key(&to_s) {
                        let name_address_index_snap = name_address_index.lock().await;
                        let target_addr_opt = name_address_index_snap.get(&to_s);
                        match target_addr_opt {
                            Option::None => {}
                            Option::Some(target_addr) => {
                                if target_addr != & addr {
                                    let peers_snap = peers.lock().await;
                                    let tx_opt = peers_snap.get(target_addr);
                                    match tx_opt {
                                        Option::None => {}
                                        Option::Some(tx) => {
                                            let _ = tx.send(message_clone.clone()).await;
                                            flag = true;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // send to to_s failed, send to everyone
                    if flag == false && !to_s.eq(&String::from("LOGINFO")) {
                        for (peer_addr, tx) in peers.lock().await.iter() {
                            /* do not send messages to the sending robot */
                            if peer_addr != &addr {
                                let _ = tx.send(message_clone.clone()).await;
                            }
                        }
                    }
                },
                Err(_) => break
            },
            _ = &mut forward => break
        }
    }
    {
        peers.lock().await.remove(&addr);
    }
    log::info!("{} disconnected from message router", addr);
}

pub enum Action {
    Subscribe(oneshot::Sender<broadcast::Receiver<(SocketAddr, LuaType)>>),
}

pub async fn new(addr: SocketAddr, mut requests_rx: mpsc::Receiver<Action>) -> io::Result<()> {
    
    let listener = TcpListener::bind(addr).await?;
    log::info!("Message router running on: {:?}", listener.local_addr());
    /* create an atomic map of all peers */
    let peers = Peers::default();
    /* create an index of robot name and address */
    let name_address_index = NameAddressIndex::default();
    /* update channel (for the journal) */
    let (updates_tx, _) = broadcast::channel(32);
    /* start the main loop */
    loop {
        tokio::select! {
            result = listener.accept() => match result {
                Ok((stream, addr)) => {
                    let peers = Arc::clone(&peers);
                    let name_address_index = Arc::clone(&name_address_index);
                    /* spawn a handler for the newly connected client */
                    tokio::spawn(client_handler(stream, addr, peers, name_address_index, updates_tx.clone()));
                }
                Err(err) => {
                    log::error!("Error accepting incoming connection: {}", err);
                }
            },
            request = requests_rx.recv() => match request {
                Some(action) => match action {
                    Action::Subscribe(callback) => {
                        let _ = callback.send(updates_tx.subscribe());
                    },
                },
                None => break,
            }
        }
    }
    Ok(())
}
