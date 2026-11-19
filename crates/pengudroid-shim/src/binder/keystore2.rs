//! Android 17 Keystore2 AIDL backend with explicitly software security level.
//!
//! The framework keeps owning its JCA provider and key/cipher SPI classes. This
//! module implements their binder backend for app-scoped AES keys. It is not a
//! TEE: raw key material is protected only by the app-private host directory.

use std::collections::BTreeMap;
use std::ffi::{c_int, c_void, CString};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use super::{
    after_interface_token, handle_for, name_for_handle, push_flat_handle, string16, Reply,
    TransactionData,
};

pub(super) const SERVICE: &str = "android.system.keystore2.IKeystoreService/default";
const LEVEL: &str = "pengudroid.keystore2.security_level.software";
const OPERATION_PREFIX: &str = "pengudroid.keystore2.operation.";
const SERVICE_DESCRIPTOR: &str = "android.system.keystore2.IKeystoreService";
const LEVEL_DESCRIPTOR: &str = "android.system.keystore2.IKeystoreSecurityLevel";
const OPERATION_DESCRIPTOR: &str = "android.system.keystore2.IKeystoreOperation";
const INTERFACE_HASH: &str = "b115fcb5d111eb616a65f0f32e0c2cef131575ec";
const INTERFACE_VERSION: i32 = 6;
const INTERFACE_VERSION_TRANSACTION: u32 = 0x00ff_ffff;
const INTERFACE_HASH_TRANSACTION: u32 = 0x00ff_fffe;

const RESPONSE_SYSTEM_ERROR: i32 = 4;
const RESPONSE_PERMISSION_DENIED: i32 = 6;
const RESPONSE_KEY_NOT_FOUND: i32 = 7;
const RESPONSE_VALUE_CORRUPTED: i32 = 8;
const RESPONSE_INVALID_ARGUMENT: i32 = 20;
const RESPONSE_TOO_MUCH_DATA: i32 = 21;
const RESPONSE_INFO_NOT_AVAILABLE: i32 = 28;
const KEYMINT_UNSUPPORTED_PURPOSE: i32 = -2;
const KEYMINT_UNSUPPORTED_PADDING_MODE: i32 = -10;
const KEYMINT_VERIFICATION_FAILED: i32 = -30;
const KEYMINT_UNSUPPORTED_TAG: i32 = -39;

const ALGORITHM_AES: i32 = 32;
const PURPOSE_ENCRYPT: i32 = 0;
const PURPOSE_DECRYPT: i32 = 1;
const BLOCK_MODE_GCM: i32 = 32;
const PADDING_NONE: i32 = 1;

const TAG_ALGORITHM: i32 = 0x1000_0002;
const TAG_KEY_SIZE: i32 = 0x3000_0003;
const TAG_PURPOSE: i32 = 0x2000_0001;
const TAG_BLOCK_MODE: i32 = 0x2000_0004;
const TAG_PADDING: i32 = 0x2000_0006;
const TAG_CALLER_NONCE: i32 = 0x7000_0007;
const TAG_NONCE: i32 = 0x9000_03e9i64 as i32;
const TAG_MAC_LENGTH: i32 = 0x3000_03eb;
const TAG_ORIGIN: i32 = 0x1000_00be;
const TAG_USER_AUTH_TYPE: i32 = 0x1000_01f8;
const TAG_AUTH_TIMEOUT: i32 = 0x3000_01f9;
const TAG_ATTESTATION_CHALLENGE: i32 = 0x9000_02c4i64 as i32;

const MAX_KEYS: usize = 512;
const MAX_PARAMS: usize = 256;
const MAX_PARAM_BYTES: usize = 64 * 1024;
const MAX_INPUT: usize = 16 * 1024 * 1024;
const MAX_AAD: usize = 1024 * 1024;
const MAX_OPERATIONS: usize = 64;
const MAX_STORE_BYTES: u64 = 16 * 1024 * 1024;

pub(super) fn descriptor_for_name(name: &str) -> Option<&'static str> {
    match name {
        SERVICE => Some(SERVICE_DESCRIPTOR),
        LEVEL => Some(LEVEL_DESCRIPTOR),
        _ if operation_id(name).is_some() => Some(OPERATION_DESCRIPTOR),
        _ => None,
    }
}

fn operation_id(name: &str) -> Option<u64> {
    name.strip_prefix(OPERATION_PREFIX)?.parse().ok()
}

#[derive(Debug)]
struct Error {
    code: i32,
    message: &'static str,
}

type Result<T> = std::result::Result<T, Error>;

fn error<T>(code: i32, message: &'static str) -> Result<T> {
    Err(Error { code, message })
}

impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self { code: RESPONSE_SYSTEM_ERROR, message: "software keystore I/O failure" }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self { Self { data, pos: 0 } }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(length).filter(|end| *end <= self.data.len())
            .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "truncated keystore parcel" })?;
        let value = &self.data[self.pos..end];
        self.pos = end;
        Ok(value)
    }

    fn align4(&mut self) -> Result<()> {
        let aligned = (self.pos + 3) & !3;
        self.take(aligned - self.pos)?;
        Ok(())
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn string(&mut self) -> Result<Option<String>> {
        let length = self.i32()?;
        if length == -1 { return Ok(None); }
        let length = usize::try_from(length).ok()
            .filter(|length| *length <= 4096)
            .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "invalid keystore string" })?;
        let bytes = self.take(length.checked_mul(2).ok_or(Error {
            code: RESPONSE_INVALID_ARGUMENT, message: "keystore string overflow"
        })?)?;
        let units: Vec<u16> = bytes.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        if self.take(2)? != [0, 0] {
            return error(RESPONSE_INVALID_ARGUMENT, "unterminated keystore string");
        }
        self.align4()?;
        String::from_utf16(&units).map(Some).map_err(|_| Error {
            code: RESPONSE_INVALID_ARGUMENT, message: "invalid keystore UTF-16"
        })
    }

    fn bytes(&mut self) -> Result<Option<Vec<u8>>> {
        let length = self.i32()?;
        if length == -1 { return Ok(None); }
        let length = usize::try_from(length).ok()
            .filter(|length| *length <= MAX_INPUT)
            .ok_or(Error { code: RESPONSE_TOO_MUCH_DATA, message: "keystore byte array too large" })?;
        let bytes = self.take(length)?.to_vec();
        self.align4()?;
        Ok(Some(bytes))
    }

    fn typed_raw(&mut self) -> Result<Option<Vec<u8>>> {
        if self.i32()? == 0 { return Ok(None); }
        let start = self.pos;
        let size = self.i32()?;
        if size < 4 {
            return error(RESPONSE_INVALID_ARGUMENT, "invalid keystore parcelable size");
        }
        let end = start.checked_add(size as usize).filter(|end| *end <= self.data.len())
            .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "truncated keystore parcelable" })?;
        self.pos = end;
        Ok(Some(self.data[start..end].to_vec()))
    }
}

struct Out {
    parcel: Vec<u8>,
    objects: Vec<u64>,
}

impl Out {
    fn new() -> Self {
        let mut out = Self { parcel: Vec::new(), objects: Vec::new() };
        out.i32(0); // AIDL Status: no exception
        out
    }

    fn i32(&mut self, value: i32) { self.parcel.extend_from_slice(&value.to_le_bytes()); }
    fn u32(&mut self, value: u32) { self.parcel.extend_from_slice(&value.to_le_bytes()); }
    fn i64(&mut self, value: i64) { self.parcel.extend_from_slice(&value.to_le_bytes()); }

    fn bytes(&mut self, value: Option<&[u8]>) {
        match value {
            None => self.i32(-1),
            Some(value) => {
                self.i32(value.len() as i32);
                self.parcel.extend_from_slice(value);
                while self.parcel.len() % 4 != 0 { self.parcel.push(0); }
            }
        }
    }

    fn string(&mut self, value: &str) { self.parcel.extend_from_slice(&string16(value)); }

    fn binder(&mut self, name: &str) {
        push_flat_handle(&mut self.parcel, &mut self.objects, handle_for(name));
    }

    fn sized(&mut self, fields: impl FnOnce(&mut Self)) {
        let start = self.parcel.len();
        self.i32(0);
        fields(self);
        let size = (self.parcel.len() - start) as i32;
        self.parcel[start..start + 4].copy_from_slice(&size.to_le_bytes());
    }

    fn typed(&mut self, fields: impl FnOnce(&mut Self)) {
        self.i32(1);
        self.sized(fields);
    }

    fn finish(mut self) -> Reply {
        while self.parcel.len() % 8 != 0 { self.parcel.push(0); }
        Reply { parcel: self.parcel, objects: self.objects, flags: 0 }
    }
}

fn exception(error: Error) -> Reply {
    let mut out = Out { parcel: Vec::new(), objects: Vec::new() };
    out.i32(-8); // Parcel.EX_SERVICE_SPECIFIC
    out.string(error.message);
    out.i32(error.code);
    out.finish()
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct KeyId {
    domain: i32,
    nspace: i64,
    alias: String,
}

#[derive(Clone, Debug)]
struct Descriptor {
    id: KeyId,
    blob: Option<Vec<u8>>,
}

fn parse_descriptor(body: &[u8]) -> Result<Descriptor> {
    let mut reader = Reader::new(body);
    let size = reader.i32()?;
    if size < 4 || size as usize > body.len() {
        return error(RESPONSE_INVALID_ARGUMENT, "invalid KeyDescriptor size");
    }
    let domain = reader.i32()?;
    let nspace = reader.i64()?;
    let alias = reader.string()?.unwrap_or_default();
    let blob = reader.bytes()?;
    if alias.len() > 4096 || alias.as_bytes().contains(&0) {
        return error(RESPONSE_INVALID_ARGUMENT, "invalid key alias");
    }
    Ok(Descriptor { id: KeyId { domain, nspace, alias }, blob })
}

fn read_descriptor(reader: &mut Reader<'_>) -> Result<Option<Descriptor>> {
    reader.typed_raw()?.map(|body| parse_descriptor(&body)).transpose()
}

#[derive(Clone, Debug)]
enum ParameterValue {
    Integer(i32),
    Long(i64),
    Bool(bool),
    Bytes(Vec<u8>),
    Other,
}

#[derive(Clone, Debug)]
struct Parameter {
    raw: Vec<u8>,
    tag: i32,
    value: ParameterValue,
}

fn parse_parameter(body: Vec<u8>) -> Result<Parameter> {
    let mut reader = Reader::new(&body);
    let size = reader.i32()?;
    if size < 4 || size as usize > body.len() {
        return error(RESPONSE_INVALID_ARGUMENT, "invalid KeyParameter size");
    }
    let tag = reader.i32()?;
    // KeyParameterValue is an AIDL union: writeTypedObject contributes only
    // the presence word, then the union tag/value (there is no size prefix).
    if reader.i32()? == 0 {
        return error(RESPONSE_INVALID_ARGUMENT, "missing KeyParameterValue");
    }
    let union_tag = reader.i32()?;
    let value = match union_tag {
        1..=9 | 11 | 15 => ParameterValue::Integer(reader.i32()?),
        10 => ParameterValue::Bool(reader.i32()? != 0),
        12 | 13 => ParameterValue::Long(reader.i64()?),
        14 => ParameterValue::Bytes(reader.bytes()?.unwrap_or_default()),
        _ => ParameterValue::Other,
    };
    Ok(Parameter { raw: body, tag, value })
}

fn read_parameters(reader: &mut Reader<'_>) -> Result<Vec<Parameter>> {
    let count = reader.i32()?;
    let count = usize::try_from(count).ok().filter(|count| *count <= MAX_PARAMS)
        .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "invalid key parameter count" })?;
    let mut params = Vec::with_capacity(count);
    for _ in 0..count {
        let raw = reader.typed_raw()?
            .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "null key parameter" })?;
        params.push(parse_parameter(raw)?);
    }
    Ok(params)
}

fn parameter_bytes(parameter: &Parameter) -> Result<Vec<u8>> {
    match parameter.value {
        ParameterValue::Bytes(ref bytes) => Ok(bytes.clone()),
        _ => error(RESPONSE_INVALID_ARGUMENT, "key parameter is not a byte array"),
    }
}

fn parameter_int(parameter: &Parameter) -> Result<i32> {
    match parameter.value {
        ParameterValue::Integer(value) => Ok(value),
        _ => error(RESPONSE_INVALID_ARGUMENT, "key parameter is not an integer"),
    }
}

fn find_int(params: &[Parameter], tag: i32) -> Option<i32> {
    params.iter().find(|parameter| parameter.tag == tag)
        .and_then(|parameter| match parameter.value {
            ParameterValue::Integer(value) => Some(value),
            _ => None,
        })
}

fn find_bytes(params: &[Parameter], tag: i32) -> Option<Vec<u8>> {
    params.iter().find(|parameter| parameter.tag == tag)
        .and_then(|parameter| match parameter.value {
            ParameterValue::Bytes(ref value) => Some(value.clone()),
            _ => None,
        })
}

fn encode_parameters(params: &[Parameter]) -> Vec<u8> {
    let mut out = Out { parcel: Vec::new(), objects: Vec::new() };
    out.i32(params.len() as i32);
    for parameter in params {
        out.i32(1);
        out.parcel.extend_from_slice(&parameter.raw);
    }
    out.parcel
}

fn decode_parameters(data: &[u8]) -> Result<Vec<Parameter>> {
    read_parameters(&mut Reader::new(data))
}

fn enum_parameter(tag: i32, union_tag: i32, value: i32) -> Parameter {
    let mut value_out = Out { parcel: Vec::new(), objects: Vec::new() };
    value_out.i32(union_tag);
    value_out.i32(value);
    let mut out = Out { parcel: Vec::new(), objects: Vec::new() };
    out.sized(|out| {
        out.i32(tag);
        out.i32(1);
        out.parcel.extend_from_slice(&value_out.parcel);
    });
    Parameter { raw: out.parcel, tag, value: ParameterValue::Integer(value) }
}

fn blob_parameter(tag: i32, value: Vec<u8>) -> Parameter {
    let mut out = Out { parcel: Vec::new(), objects: Vec::new() };
    out.sized(|out| {
        out.i32(tag);
        out.i32(1);
        out.i32(14); // KeyParameterValue.blob
        out.bytes(Some(&value));
    });
    Parameter { raw: out.parcel, tag, value: ParameterValue::Bytes(value) }
}

#[derive(Clone)]
struct KeyRecord {
    descriptor: Descriptor,
    params: Vec<Parameter>,
    key: Vec<u8>,
    created_ms: i64,
    storage_id: Vec<u8>,
}

impl Drop for KeyRecord {
    fn drop(&mut self) {
        self.key.fill(0);
    }
}

impl KeyRecord {
    fn algorithm(&self) -> Option<i32> { find_int(&self.params, TAG_ALGORITHM) }
    fn key_size(&self) -> Option<i32> { find_int(&self.params, TAG_KEY_SIZE) }

    fn key_id(&self) -> i64 {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.storage_id[..8]);
        // KEY_ID namespaces are positive, stable and independent of the alias
        // namespace supplied by KeyGenParameterSpec.
        ((u64::from_le_bytes(bytes) & (i64::MAX as u64)) | 1) as i64
    }

    fn operation_descriptor(&self) -> Descriptor {
        Descriptor {
            id: KeyId { domain: 4, nspace: self.key_id(), alias: String::new() },
            blob: None,
        }
    }

    fn allows_purpose(&self, purpose: i32) -> bool {
        self.params.iter().any(|parameter| parameter.tag == TAG_PURPOSE
            && matches!(parameter.value, ParameterValue::Integer(value) if value == purpose))
    }
}

fn storage_root() -> PathBuf {
    std::env::var_os("PENGUDROID_PREFIX").map(PathBuf::from)
        .or_else(|| std::env::var_os("ANDROID_DATA").map(PathBuf::from))
        .unwrap_or_else(|| std::env::temp_dir().join("pengudroid-keystore2"))
        .join("keystore2")
}

fn push_field(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn take_field<'a>(data: &mut &'a [u8], maximum: usize) -> Result<&'a [u8]> {
    if data.len() < 4 { return error(RESPONSE_VALUE_CORRUPTED, "truncated software key store"); }
    let length = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    *data = &data[4..];
    if length > maximum || data.len() < length {
        return error(RESPONSE_VALUE_CORRUPTED, "invalid software key store field");
    }
    let (value, rest) = data.split_at(length);
    *data = rest;
    Ok(value)
}

fn encode_store(keys: &BTreeMap<KeyId, KeyRecord>) -> Vec<u8> {
    let mut out = b"PDKS2\0\x01".to_vec();
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for record in keys.values() {
        out.extend_from_slice(&record.descriptor.id.domain.to_le_bytes());
        out.extend_from_slice(&record.descriptor.id.nspace.to_le_bytes());
        push_field(&mut out, record.descriptor.id.alias.as_bytes());
        push_field(&mut out, &record.storage_id);
        let params = encode_parameters(&record.params);
        push_field(&mut out, &params);
        push_field(&mut out, &record.key);
        out.extend_from_slice(&record.created_ms.to_le_bytes());
    }
    out
}

fn decode_store(mut data: &[u8]) -> Result<BTreeMap<KeyId, KeyRecord>> {
    if data.len() < 11 || &data[..7] != b"PDKS2\0\x01" {
        return error(RESPONSE_VALUE_CORRUPTED, "unknown software key store format");
    }
    data = &data[7..];
    let count = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
    data = &data[4..];
    if count > MAX_KEYS { return error(RESPONSE_VALUE_CORRUPTED, "too many stored keys"); }
    let mut keys = BTreeMap::new();
    for _ in 0..count {
        if data.len() < 12 { return error(RESPONSE_VALUE_CORRUPTED, "truncated stored key"); }
        let domain = i32::from_le_bytes(data[..4].try_into().unwrap());
        let nspace = i64::from_le_bytes(data[4..12].try_into().unwrap());
        data = &data[12..];
        let alias = String::from_utf8(take_field(&mut data, 4096)?.to_vec())
            .map_err(|_| Error { code: RESPONSE_VALUE_CORRUPTED, message: "invalid stored key alias" })?;
        let storage_id = take_field(&mut data, 64)?.to_vec();
        if storage_id.len() < 8 {
            return error(RESPONSE_VALUE_CORRUPTED, "invalid stored key identifier");
        }
        let params = decode_parameters(take_field(&mut data, MAX_PARAM_BYTES)?)?;
        let key = take_field(&mut data, 64)?.to_vec();
        if data.len() < 8 { return error(RESPONSE_VALUE_CORRUPTED, "truncated stored key timestamp"); }
        let created_ms = i64::from_le_bytes(data[..8].try_into().unwrap());
        data = &data[8..];
        let descriptor = Descriptor {
            id: KeyId { domain, nspace, alias },
            blob: Some(storage_id.clone()),
        };
        let record = KeyRecord { descriptor, params, key, created_ms, storage_id };
        keys.insert(record.descriptor.id.clone(), record);
    }
    if !data.is_empty() { return error(RESPONSE_VALUE_CORRUPTED, "trailing software key store data"); }
    Ok(keys)
}

struct Store {
    loaded: bool,
    root: PathBuf,
    file: PathBuf,
    keys: BTreeMap<KeyId, KeyRecord>,
}

impl Store {
    fn new() -> Self {
        let root = storage_root();
        let file = root.join("software-keys-v1");
        Self { loaded: false, root, file, keys: BTreeMap::new() }
    }

    fn load(&mut self) -> Result<()> {
        if self.loaded { return Ok(()); }
        std::fs::create_dir_all(&self.root)?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))?;
        match std::fs::metadata(&self.file) {
            Ok(metadata) => {
                if metadata.len() > MAX_STORE_BYTES {
                    return error(RESPONSE_VALUE_CORRUPTED, "software key store is too large");
                }
                self.keys = decode_store(&std::fs::read(&self.file)?)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.loaded = true;
        Ok(())
    }

    fn persist(&self) -> Result<()> {
        let bytes = encode_store(&self.keys);
        let temporary = self.root.join("software-keys-v1.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
        std::fs::rename(&temporary, &self.file)?;
        Ok(())
    }

    fn lookup(&mut self, descriptor: &Descriptor) -> Result<KeyRecord> {
        self.load()?;
        if descriptor.id.domain == 4 {
            if let Some(record) = self.keys.values().find(|record| record.key_id() == descriptor.id.nspace) {
                return Ok(record.clone());
            }
        }
        if let Some(record) = self.keys.get(&descriptor.id) { return Ok(record.clone()); }
        if let Some(blob) = &descriptor.blob {
            if let Some(record) = self.keys.values().find(|record| &record.storage_id == blob) {
                return Ok(record.clone());
            }
        }
        error(RESPONSE_KEY_NOT_FOUND, "key not found")
    }

    fn insert(&mut self, record: KeyRecord) -> Result<()> {
        self.load()?;
        if !self.keys.contains_key(&record.descriptor.id) && self.keys.len() >= MAX_KEYS {
            return error(RESPONSE_TOO_MUCH_DATA, "software key store is full");
        }
        let id = record.descriptor.id.clone();
        let old = self.keys.insert(id.clone(), record);
        if let Err(error) = self.persist() {
            if let Some(old) = old {
                self.keys.insert(old.descriptor.id.clone(), old);
            } else {
                self.keys.remove(&id);
            }
            return Err(error);
        }
        Ok(())
    }

    fn remove(&mut self, descriptor: &Descriptor) -> Result<()> {
        self.load()?;
        let record = self.lookup(descriptor)?;
        self.keys.remove(&record.descriptor.id);
        self.persist()
    }
}

static STORE: LazyLock<Mutex<Store>> = LazyLock::new(|| Mutex::new(Store::new()));

struct Operation {
    key: Vec<u8>,
    encrypt: bool,
    iv: Vec<u8>,
    tag_bytes: usize,
    aad: Vec<u8>,
    input: Vec<u8>,
}

impl Drop for Operation {
    fn drop(&mut self) {
        self.key.fill(0);
        self.aad.fill(0);
        self.input.fill(0);
    }
}

static OPERATIONS: LazyLock<Mutex<BTreeMap<u64, Operation>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

fn random(bytes: &mut [u8]) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let result = unsafe {
            libc::getrandom(bytes[offset..].as_mut_ptr().cast(), bytes.len() - offset, 0)
        };
        if result < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted { continue; }
            return error(RESPONSE_SYSTEM_ERROR, "operating-system random generator failed");
        }
        if result == 0 { return error(RESPONSE_SYSTEM_ERROR, "operating-system random generator stalled"); }
        offset += result as usize;
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default().as_millis().min(i64::MAX as u128) as i64
}

fn write_descriptor(out: &mut Out, descriptor: &Descriptor) {
    out.typed(|out| {
        out.i32(descriptor.id.domain);
        out.i64(descriptor.id.nspace);
        out.string(&descriptor.id.alias);
        out.bytes(descriptor.blob.as_deref());
    });
}

fn write_authorization(out: &mut Out, parameter: &Parameter) {
    out.i32(1);
    out.sized(|out| {
        out.i32(0); // SECURITY_LEVEL_SOFTWARE
        out.i32(1);
        out.parcel.extend_from_slice(&parameter.raw);
    });
}

fn write_metadata(out: &mut Out, record: &KeyRecord) {
    out.typed(|out| {
        write_descriptor(out, &record.operation_descriptor());
        out.i32(0); // keySecurityLevel = SOFTWARE, never claim TEE
        let count = record.params.len() + 1;
        out.i32(count as i32);
        for parameter in &record.params { write_authorization(out, parameter); }
        let origin = enum_parameter(TAG_ORIGIN, 6, 0); // KeyOrigin.GENERATED
        write_authorization(out, &origin);
        out.bytes(None); // certificate
        out.bytes(None); // certificateChain
        out.i64(record.created_ms);
    });
}

fn write_key_entry(out: &mut Out, record: &KeyRecord) {
    out.typed(|out| {
        out.binder(LEVEL);
        write_metadata(out, record);
    });
}

fn generate_key(descriptor: Descriptor, params: Vec<Parameter>, imported: Option<Vec<u8>>) -> Result<KeyRecord> {
    if descriptor.id.alias.is_empty() || descriptor.id.domain != 0 && descriptor.id.domain != 2 {
        return error(RESPONSE_INVALID_ARGUMENT, "only app/SELinux alias keys are supported");
    }
    if params.iter().any(|parameter| parameter.tag == TAG_ATTESTATION_CHALLENGE) {
        return error(RESPONSE_INVALID_ARGUMENT, "key attestation is not supported");
    }
    if params.iter().any(|parameter| parameter.tag == TAG_USER_AUTH_TYPE || parameter.tag == TAG_AUTH_TIMEOUT) {
        return error(RESPONSE_INVALID_ARGUMENT, "user-authenticated keys require an authenticator backend");
    }
    let algorithm = find_int(&params, TAG_ALGORITHM)
        .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "missing key algorithm" })?;
    if algorithm != ALGORITHM_AES {
        return error(RESPONSE_INVALID_ARGUMENT, "software backend currently supports AES only");
    }
    let key_size = find_int(&params, TAG_KEY_SIZE).unwrap_or(256);
    if !matches!(key_size, 128 | 192 | 256) {
        return error(RESPONSE_INVALID_ARGUMENT, "invalid AES key size");
    }
    if !params.iter().any(|parameter| parameter.tag == TAG_BLOCK_MODE
        && matches!(parameter.value, ParameterValue::Integer(value) if value == BLOCK_MODE_GCM)) {
        return error(KEYMINT_UNSUPPORTED_TAG, "only AES-GCM keys are supported");
    }
    if params.iter().any(|parameter| parameter.tag == TAG_PADDING
        && matches!(parameter.value, ParameterValue::Integer(value) if value != PADDING_NONE)) {
        return error(KEYMINT_UNSUPPORTED_PADDING_MODE, "only NoPadding is supported");
    }
    let is_import = imported.is_some();
    let mut key = imported.unwrap_or_else(|| vec![0; key_size as usize / 8]);
    if key.len() * 8 != key_size as usize {
        key.fill(0);
        return error(RESPONSE_INVALID_ARGUMENT, "imported AES key size mismatch");
    }
    if !is_import { random(&mut key)?; }
    let mut storage_id = vec![0u8; 16];
    random(&mut storage_id)?;
    let descriptor = Descriptor { id: descriptor.id, blob: Some(storage_id.clone()) };
    let record = KeyRecord { descriptor, params, key, created_ms: now_ms(), storage_id };
    STORE.lock().unwrap_or_else(|e| e.into_inner()).insert(record.clone())?;
    Ok(record)
}

fn create_operation(descriptor: Descriptor, params: Vec<Parameter>) -> Result<(u64, Vec<u8>)> {
    let record = STORE.lock().unwrap_or_else(|e| e.into_inner()).lookup(&descriptor)?;
    if record.algorithm() != Some(ALGORITHM_AES) {
        return error(RESPONSE_INVALID_ARGUMENT, "operation requires an AES key");
    }
    let purpose = find_int(&params, TAG_PURPOSE)
        .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "missing operation purpose" })?;
    if !matches!(purpose, PURPOSE_ENCRYPT | PURPOSE_DECRYPT) {
        return error(KEYMINT_UNSUPPORTED_PURPOSE, "only encrypt/decrypt operations are supported");
    }
    if !record.allows_purpose(purpose) {
        return error(RESPONSE_PERMISSION_DENIED, "key does not authorize this purpose");
    }
    let supplied_iv = find_bytes(&params, TAG_NONCE);
    let encrypt = purpose == PURPOSE_ENCRYPT;
    let iv = match supplied_iv {
        Some(iv) => iv,
        None if encrypt => {
            let mut iv = vec![0u8; 12];
            random(&mut iv)?;
            iv
        }
        None => return error(RESPONSE_INVALID_ARGUMENT, "decryption requires a GCM nonce"),
    };
    if iv.is_empty() || iv.len() > 64 {
        return error(RESPONSE_INVALID_ARGUMENT, "invalid GCM nonce length");
    }
    let tag_bits = find_int(&params, TAG_MAC_LENGTH)
        .or_else(|| find_int(&record.params, TAG_MAC_LENGTH)).unwrap_or(128);
    if !(96..=128).contains(&tag_bits) || tag_bits % 8 != 0 {
        return error(RESPONSE_INVALID_ARGUMENT, "unsupported GCM tag length");
    }
    let operation = Operation {
        key: record.key.clone(), encrypt, iv: iv.clone(),
        tag_bytes: tag_bits as usize / 8, aad: Vec::new(), input: Vec::new(),
    };
    let id = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
    if id == 0 { return error(RESPONSE_SYSTEM_ERROR, "operation id exhausted"); }
    let mut operations = OPERATIONS.lock().unwrap_or_else(|e| e.into_inner());
    if operations.len() >= MAX_OPERATIONS {
        return error(RESPONSE_TOO_MUCH_DATA, "too many live keystore operations");
    }
    operations.insert(id, operation);
    Ok((id, iv))
}

fn write_operation_response(id: u64, iv: Vec<u8>) -> Reply {
    let mut out = Out::new();
    out.typed(|out| {
        out.binder(&format!("{OPERATION_PREFIX}{id}"));
        out.i32(0); // no OperationChallenge
        out.typed(|out| { // KeyParameters
            let nonce = blob_parameter(TAG_NONCE, iv);
            let params = encode_parameters(&[nonce]);
            out.parcel.extend_from_slice(&params);
        });
        out.bytes(None); // upgradedBlob
    });
    out.finish()
}

struct Crypto {
    _handle: usize,
    ctx_new: unsafe extern "C" fn() -> *mut c_void,
    ctx_free: unsafe extern "C" fn(*mut c_void),
    aes128: unsafe extern "C" fn() -> *const c_void,
    aes192: unsafe extern "C" fn() -> *const c_void,
    aes256: unsafe extern "C" fn() -> *const c_void,
    encrypt_init: unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void, *const u8, *const u8) -> c_int,
    encrypt_update: unsafe extern "C" fn(*mut c_void, *mut u8, *mut c_int, *const u8, c_int) -> c_int,
    encrypt_final: unsafe extern "C" fn(*mut c_void, *mut u8, *mut c_int) -> c_int,
    decrypt_init: unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void, *const u8, *const u8) -> c_int,
    decrypt_update: unsafe extern "C" fn(*mut c_void, *mut u8, *mut c_int, *const u8, c_int) -> c_int,
    decrypt_final: unsafe extern "C" fn(*mut c_void, *mut u8, *mut c_int) -> c_int,
    ctrl: unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut c_void) -> c_int,
}

unsafe fn crypto_symbol<T: Copy>(handle: *mut c_void, name: &str) -> std::result::Result<T, String> {
    let name = CString::new(name).unwrap();
    let pointer = libc::dlsym(handle, name.as_ptr());
    if pointer.is_null() { return Err(format!("host OpenSSL symbol unavailable")); }
    if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
        return Err("invalid host OpenSSL function pointer shape".into());
    }
    Ok(std::mem::transmute_copy(&pointer))
}

impl Crypto {
    fn load() -> std::result::Result<Self, String> {
        let candidates: &[&str] = if cfg!(target_arch = "aarch64") {
            &["/usr/lib/libcrypto.so.1.1", "/lib64/libcrypto.so.3", "libcrypto.so.1.1", "libcrypto.so.3"]
        } else {
            &["/usr/lib/x86_64-linux-gnu/libcrypto.so.3", "/lib64/libcrypto.so.3", "libcrypto.so.3", "libcrypto.so.1.1"]
        };
        for candidate in candidates {
            let path = CString::new(*candidate).unwrap();
            let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
            if handle.is_null() { continue; }
            let loaded = unsafe {
                Ok(Self {
                    _handle: handle as usize,
                    ctx_new: crypto_symbol(handle, "EVP_CIPHER_CTX_new")?,
                    ctx_free: crypto_symbol(handle, "EVP_CIPHER_CTX_free")?,
                    aes128: crypto_symbol(handle, "EVP_aes_128_gcm")?,
                    aes192: crypto_symbol(handle, "EVP_aes_192_gcm")?,
                    aes256: crypto_symbol(handle, "EVP_aes_256_gcm")?,
                    encrypt_init: crypto_symbol(handle, "EVP_EncryptInit_ex")?,
                    encrypt_update: crypto_symbol(handle, "EVP_EncryptUpdate")?,
                    encrypt_final: crypto_symbol(handle, "EVP_EncryptFinal_ex")?,
                    decrypt_init: crypto_symbol(handle, "EVP_DecryptInit_ex")?,
                    decrypt_update: crypto_symbol(handle, "EVP_DecryptUpdate")?,
                    decrypt_final: crypto_symbol(handle, "EVP_DecryptFinal_ex")?,
                    ctrl: crypto_symbol(handle, "EVP_CIPHER_CTX_ctrl")?,
                })
            };
            if loaded.is_ok() { return loaded; }
        }
        Err("host OpenSSL AES-GCM unavailable".into())
    }
}

static CRYPTO: LazyLock<std::result::Result<Crypto, String>> = LazyLock::new(Crypto::load);

struct CipherContext(*mut c_void);
impl Drop for CipherContext {
    fn drop(&mut self) {
        if let Ok(crypto) = &*CRYPTO { unsafe { (crypto.ctx_free)(self.0) } }
    }
}

fn aes_gcm(operation: &Operation) -> Result<Vec<u8>> {
    let crypto = CRYPTO.as_ref().map_err(|_| Error {
        code: RESPONSE_SYSTEM_ERROR, message: "host OpenSSL AES-GCM unavailable"
    })?;
    if operation.input.len() > c_int::MAX as usize {
        return error(RESPONSE_TOO_MUCH_DATA, "AES-GCM input is too large");
    }
    let cipher = match operation.key.len() {
        16 => unsafe { (crypto.aes128)() },
        24 => unsafe { (crypto.aes192)() },
        32 => unsafe { (crypto.aes256)() },
        _ => return error(RESPONSE_INVALID_ARGUMENT, "invalid AES key length"),
    };
    let context = CipherContext(unsafe { (crypto.ctx_new)() });
    if context.0.is_null() { return error(RESPONSE_SYSTEM_ERROR, "OpenSSL context allocation failed"); }
    let mut output = vec![0u8; operation.input.len() + 16];
    let mut output_len: c_int = 0;
    let mut final_len: c_int = 0;
    unsafe {
        let init_result = if operation.encrypt {
            (crypto.encrypt_init)(context.0, cipher, std::ptr::null(), std::ptr::null(), std::ptr::null())
        } else {
            (crypto.decrypt_init)(context.0, cipher, std::ptr::null(), std::ptr::null(), std::ptr::null())
        };
        if init_result != 1
            || (crypto.ctrl)(context.0, 0x9, operation.iv.len() as c_int, std::ptr::null_mut()) != 1 {
            return error(RESPONSE_SYSTEM_ERROR, "OpenSSL GCM initialization failed");
        }
        let key_iv_result = if operation.encrypt {
            (crypto.encrypt_init)(context.0, std::ptr::null(), std::ptr::null(), operation.key.as_ptr(), operation.iv.as_ptr())
        } else {
            (crypto.decrypt_init)(context.0, std::ptr::null(), std::ptr::null(), operation.key.as_ptr(), operation.iv.as_ptr())
        };
        if key_iv_result != 1 { return error(RESPONSE_SYSTEM_ERROR, "OpenSSL key/nonce initialization failed"); }
        if !operation.aad.is_empty() {
            let aad_result = if operation.encrypt {
                (crypto.encrypt_update)(context.0, std::ptr::null_mut(), &mut output_len,
                    operation.aad.as_ptr(), operation.aad.len() as c_int)
            } else {
                (crypto.decrypt_update)(context.0, std::ptr::null_mut(), &mut output_len,
                    operation.aad.as_ptr(), operation.aad.len() as c_int)
            };
            if aad_result != 1 { return error(RESPONSE_SYSTEM_ERROR, "OpenSSL GCM AAD failed"); }
        }
    }

    if operation.encrypt {
        unsafe {
            if !operation.input.is_empty()
                && (crypto.encrypt_update)(context.0, output.as_mut_ptr(), &mut output_len,
                    operation.input.as_ptr(), operation.input.len() as c_int) != 1 {
                return error(RESPONSE_SYSTEM_ERROR, "OpenSSL GCM encryption failed");
            }
            let offset = output_len.max(0) as usize;
            if (crypto.encrypt_final)(context.0, output[offset..].as_mut_ptr(), &mut final_len) != 1 {
                return error(RESPONSE_SYSTEM_ERROR, "OpenSSL GCM finalization failed");
            }
            let total = offset + final_len.max(0) as usize;
            output.truncate(total);
            let mut tag = vec![0u8; operation.tag_bytes];
            if (crypto.ctrl)(context.0, 0x10, tag.len() as c_int, tag.as_mut_ptr().cast()) != 1 {
                return error(RESPONSE_SYSTEM_ERROR, "OpenSSL GCM tag generation failed");
            }
            output.extend_from_slice(&tag);
        }
        Ok(output)
    } else {
        if operation.input.len() < operation.tag_bytes {
            return error(KEYMINT_VERIFICATION_FAILED, "GCM authentication tag is missing");
        }
        let ciphertext_len = operation.input.len() - operation.tag_bytes;
        let (ciphertext, tag) = operation.input.split_at(ciphertext_len);
        unsafe {
            if ciphertext_len != 0
                && (crypto.decrypt_update)(context.0, output.as_mut_ptr(), &mut output_len,
                    ciphertext.as_ptr(), ciphertext_len as c_int) != 1 {
                return error(RESPONSE_SYSTEM_ERROR, "OpenSSL GCM decryption failed");
            }
            if (crypto.ctrl)(context.0, 0x11, tag.len() as c_int, tag.as_ptr() as *mut c_void) != 1 {
                return error(RESPONSE_SYSTEM_ERROR, "OpenSSL GCM tag setup failed");
            }
            let offset = output_len.max(0) as usize;
            if (crypto.decrypt_final)(context.0, output[offset..].as_mut_ptr(), &mut final_len) != 1 {
                output.fill(0);
                return error(KEYMINT_VERIFICATION_FAILED, "GCM authentication failed");
            }
            output.truncate(offset + final_len.max(0) as usize);
        }
        Ok(output)
    }
}

fn common_interface(code: u32) -> Option<Reply> {
    match code {
        INTERFACE_VERSION_TRANSACTION => {
            let mut out = Out::new();
            out.i32(INTERFACE_VERSION);
            Some(out.finish())
        }
        INTERFACE_HASH_TRANSACTION => {
            let mut out = Out::new();
            out.string(INTERFACE_HASH);
            Some(out.finish())
        }
        _ => None,
    }
}

fn service_transaction(code: u32, data: &[u8]) -> Result<Reply> {
    let mut reader = Reader::new(data);
    match code {
        1 => { // getSecurityLevel
            let level = reader.i32()?;
            if !matches!(level, 0 | 1) {
                return error(RESPONSE_INVALID_ARGUMENT, "StrongBox/unknown security level unavailable");
            }
            let mut out = Out::new();
            out.binder(LEVEL);
            Ok(out.finish())
        }
        2 => { // getKeyEntry
            let descriptor = read_descriptor(&mut reader)?
                .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "null KeyDescriptor" })?;
            let record = STORE.lock().unwrap_or_else(|e| e.into_inner()).lookup(&descriptor)?;
            let mut out = Out::new();
            write_key_entry(&mut out, &record);
            Ok(out.finish())
        }
        4 | 9 => { // listEntries/listEntriesBatched
            let domain = reader.i32()?;
            let nspace = reader.i64()?;
            if code == 9 { let _start_after = reader.string()?; }
            let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
            store.load()?;
            let descriptors: Vec<Descriptor> = store.keys.values()
                .filter(|record| record.descriptor.id.domain == domain && record.descriptor.id.nspace == nspace)
                .map(|record| record.descriptor.clone()).collect();
            let mut out = Out::new();
            out.i32(descriptors.len() as i32);
            for descriptor in descriptors { write_descriptor(&mut out, &descriptor); }
            Ok(out.finish())
        }
        5 => { // deleteKey
            let descriptor = read_descriptor(&mut reader)?
                .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "null KeyDescriptor" })?;
            STORE.lock().unwrap_or_else(|e| e.into_inner()).remove(&descriptor)?;
            Ok(Out::new().finish())
        }
        8 => { // getNumberOfEntries
            let domain = reader.i32()?;
            let nspace = reader.i64()?;
            let mut store = STORE.lock().unwrap_or_else(|e| e.into_inner());
            store.load()?;
            let count = store.keys.values().filter(|record|
                record.descriptor.id.domain == domain && record.descriptor.id.nspace == nspace).count();
            let mut out = Out::new();
            out.i32(count as i32);
            Ok(out.finish())
        }
        10 => error(RESPONSE_INFO_NOT_AVAILABLE, "attestation information unavailable"),
        _ => error(RESPONSE_INVALID_ARGUMENT, "unsupported IKeystoreService method"),
    }
}

fn level_transaction(code: u32, data: &[u8]) -> Result<Reply> {
    let mut reader = Reader::new(data);
    match code {
        1 => { // createOperation
            let descriptor = read_descriptor(&mut reader)?
                .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "null KeyDescriptor" })?;
            let params = read_parameters(&mut reader)?;
            let _forced = reader.i32()?;
            let (id, iv) = create_operation(descriptor, params)?;
            Ok(write_operation_response(id, iv))
        }
        2 | 3 => { // generateKey/importKey
            let descriptor = read_descriptor(&mut reader)?
                .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "null KeyDescriptor" })?;
            if read_descriptor(&mut reader)?.is_some() {
                return error(RESPONSE_INVALID_ARGUMENT, "key attestation is not supported");
            }
            let params = read_parameters(&mut reader)?;
            let _flags = reader.i32()?;
            let data = reader.bytes()?;
            let record = generate_key(descriptor, params, if code == 3 { data } else { None })?;
            let mut out = Out::new();
            write_metadata(&mut out, &record);
            Ok(out.finish())
        }
        6 => { // deleteKey
            let descriptor = read_descriptor(&mut reader)?
                .ok_or(Error { code: RESPONSE_INVALID_ARGUMENT, message: "null KeyDescriptor" })?;
            STORE.lock().unwrap_or_else(|e| e.into_inner()).remove(&descriptor)?;
            Ok(Out::new().finish())
        }
        _ => error(RESPONSE_INVALID_ARGUMENT, "unsupported IKeystoreSecurityLevel method"),
    }
}

fn operation_transaction(id: u64, code: u32, data: &[u8]) -> Result<Reply> {
    let mut reader = Reader::new(data);
    match code {
        1 => { // updateAad
            let input = reader.bytes()?.unwrap_or_default();
            let mut operations = OPERATIONS.lock().unwrap_or_else(|e| e.into_inner());
            let Some(operation) = operations.get_mut(&id) else {
                return error(RESPONSE_KEY_NOT_FOUND, "operation not found");
            };
            if operation.aad.len() + input.len() > MAX_AAD {
                return error(RESPONSE_TOO_MUCH_DATA, "too much GCM AAD");
            }
            operation.aad.extend_from_slice(&input);
            Ok(Out::new().finish())
        }
        2 => { // update
            let input = reader.bytes()?.unwrap_or_default();
            let mut operations = OPERATIONS.lock().unwrap_or_else(|e| e.into_inner());
            let Some(operation) = operations.get_mut(&id) else {
                return error(RESPONSE_KEY_NOT_FOUND, "operation not found");
            };
            if operation.input.len() + input.len() > MAX_INPUT {
                return error(RESPONSE_TOO_MUCH_DATA, "too much operation input");
            }
            operation.input.extend_from_slice(&input);
            let mut out = Out::new();
            out.bytes(Some(&[])); // GCM output is returned by finish
            Ok(out.finish())
        }
        3 => { // finish
            let input = reader.bytes()?.unwrap_or_default();
            let signature = reader.bytes()?;
            if signature.is_some_and(|signature| !signature.is_empty()) {
                return error(RESPONSE_INVALID_ARGUMENT, "AES-GCM does not accept a separate signature");
            }
            let mut operation = OPERATIONS.lock().unwrap_or_else(|e| e.into_inner()).remove(&id)
                .ok_or(Error { code: RESPONSE_KEY_NOT_FOUND, message: "operation not found" })?;
            if operation.input.len() + input.len() > MAX_INPUT {
                return error(RESPONSE_TOO_MUCH_DATA, "too much operation input");
            }
            operation.input.extend_from_slice(&input);
            let result = aes_gcm(&operation)?;
            let mut out = Out::new();
            out.bytes(Some(&result));
            Ok(out.finish())
        }
        4 => { // abort: idempotent, including cleanup after a successful finish
            OPERATIONS.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
            Ok(Out::new().finish())
        }
        _ => error(RESPONSE_INVALID_ARGUMENT, "unsupported IKeystoreOperation method"),
    }
}

pub(super) unsafe fn transaction(tr: &TransactionData) -> Option<Reply> {
    let name = name_for_handle(tr.target_handle)?;
    let descriptor = descriptor_for_name(&name)?;
    let data = if tr.data_size != 0 && tr.data_ptr != 0 {
        std::slice::from_raw_parts(tr.data_ptr as *const u8, tr.data_size as usize)
    } else { &[] };
    if let Some(reply) = common_interface(tr.code) { return Some(reply); }
    let Some(arguments) = after_interface_token(data, descriptor) else {
        return Some(Reply::unknown_transaction(tr, "Keystore2 interface token mismatch"));
    };
    let result = match name.as_str() {
        SERVICE => service_transaction(tr.code, arguments),
        LEVEL => level_transaction(tr.code, arguments),
        _ => operation_transaction(operation_id(&name).unwrap(), tr.code, arguments),
    };
    Some(match result {
        Ok(reply) => reply,
        Err(error) => exception(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor_parcel(domain: i32, nspace: i64, alias: &str) -> Vec<u8> {
        let mut out = Out { parcel: Vec::new(), objects: Vec::new() };
        write_descriptor(&mut out, &Descriptor {
            id: KeyId { domain, nspace, alias: alias.into() }, blob: Some(vec![1, 2, 3]),
        });
        out.parcel
    }

    #[test]
    fn descriptor_parcel_roundtrip_preserves_its_fields() {
        let parcel = descriptor_parcel(0, -1, "test-key");
        let mut reader = Reader::new(&parcel);
        let descriptor = read_descriptor(&mut reader).unwrap().unwrap();
        assert_eq!(descriptor.id, KeyId { domain: 0, nspace: -1, alias: "test-key".into() });
        assert_eq!(descriptor.blob, Some(vec![1, 2, 3]));
    }

    #[test]
    fn parameter_array_roundtrip_preserves_raw_parcelables() {
        let parameters = vec![
            enum_parameter(TAG_ALGORITHM, 1, ALGORITHM_AES),
            enum_parameter(TAG_KEY_SIZE, 11, 256),
            blob_parameter(TAG_NONCE, vec![0; 12]),
        ];
        let encoded = encode_parameters(&parameters);
        let decoded = decode_parameters(&encoded).unwrap();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[0].raw, parameters[0].raw);
        assert_eq!(decoded[1].raw, parameters[1].raw);
        assert_eq!(decoded[2].raw, parameters[2].raw);
        assert_eq!(find_int(&decoded, TAG_KEY_SIZE), Some(256));
        assert_eq!(find_bytes(&decoded, TAG_NONCE), Some(vec![0; 12]));
    }

    #[test]
    fn openssl_aes256_gcm_matches_the_nist_zero_vector() {
        let operation = Operation {
            key: vec![0; 32], encrypt: true, iv: vec![0; 12], tag_bytes: 16,
            aad: Vec::new(), input: vec![0; 16],
        };
        let encrypted = aes_gcm(&operation).unwrap();
        let expected = [
            0xce, 0xa7, 0x40, 0x3d, 0x4d, 0x60, 0x6b, 0x6e,
            0x07, 0x4e, 0xc5, 0xd3, 0xba, 0xf3, 0x9d, 0x18,
            0xd0, 0xd1, 0xc8, 0xa7, 0x99, 0x99, 0x6b, 0xf0,
            0x26, 0x5b, 0x98, 0xb5, 0xd4, 0x8a, 0xb9, 0x19,
        ];
        assert_eq!(encrypted, expected);
        let decrypt = Operation {
            key: vec![0; 32], encrypt: false, iv: vec![0; 12], tag_bytes: 16,
            aad: Vec::new(), input: encrypted,
        };
        assert_eq!(aes_gcm(&decrypt).unwrap(), vec![0; 16]);
    }

    #[test]
    fn aes_gcm_rejects_a_modified_authentication_tag() {
        let encrypt = Operation {
            key: vec![7; 32], encrypt: true, iv: vec![9; 12], tag_bytes: 16,
            aad: b"fixed-aad".to_vec(), input: b"fixed plaintext".to_vec(),
        };
        let mut encrypted = aes_gcm(&encrypt).unwrap();
        let last = encrypted.len() - 1;
        encrypted[last] ^= 1;
        let decrypt = Operation {
            key: vec![7; 32], encrypt: false, iv: vec![9; 12], tag_bytes: 16,
            aad: b"fixed-aad".to_vec(), input: encrypted,
        };
        let error = aes_gcm(&decrypt).unwrap_err();
        assert_eq!(error.code, KEYMINT_VERIFICATION_FAILED);
    }
}
