//! Calls handlers in a compiled AppleScript in-process via NSAppleScript, the
//! same Apple-event bridge py-applescript uses.

use std::fmt;

use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::msg_send;
use objc2_foundation::{
    NSAppleEventDescriptor, NSAppleScript, NSAppleScriptErrorMessage, NSAppleScriptErrorNumber,
    NSData, NSDictionary, NSNumber, NSString,
};

const fn code(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const TYPE_NULL: u32 = code(b"null");
const TYPE_TYPE: u32 = code(b"type");
const TYPE_BOOL: u32 = code(b"bool");
const TYPE_TRUE: u32 = code(b"true");
const TYPE_FALSE: u32 = code(b"fals");
const TYPE_SHORT: u32 = code(b"shor");
const TYPE_LONG: u32 = code(b"long");
const TYPE_COMP: u32 = code(b"comp");
const TYPE_DOUBLE: u32 = code(b"doub");
const TYPE_SINGLE: u32 = code(b"sing");
const TYPE_UTXT: u32 = code(b"utxt");
const TYPE_UTF8: u32 = code(b"utf8");
const TYPE_UT16: u32 = code(b"ut16");
const TYPE_TEXT: u32 = code(b"TEXT");
const TYPE_LDT: u32 = code(b"ldt ");
const TYPE_LIST: u32 = code(b"list");
const MISSING_VALUE: u32 = code(b"msng");

/// AppleScript error number for "AppleEvent timed out".
pub const ERR_TIMEOUT: i64 = -1712;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// `missing value` (or no return value)
    Missing,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    /// AppleScript dates are timezone-naive local time.
    Date(NaiveDateTime),
    List(Vec<Value>),
}

#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub number: Option<i64>,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.number {
            Some(n) => write!(f, "{} ({n})", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for Error {}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Error { message: message.into(), number: None }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn mac_epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(1904, 1, 1).unwrap().and_hms_opt(0, 0, 0).unwrap()
}

pub struct Script(Retained<NSAppleScript>);

impl Script {
    pub fn compile(source: &str) -> Result<Self> {
        let script = NSAppleScript::initWithSource(NSAppleScript::alloc(), &NSString::from_str(source))
            .ok_or_else(|| Error::new("could not create NSAppleScript"))?;
        let mut err = None;
        if !unsafe { script.compileAndReturnError(Some(&mut err)) } {
            return Err(error_from_info(err.as_deref()));
        }
        Ok(Script(script))
    }

    /// Call a user-defined handler by name with positional args.
    pub fn call(&self, handler: &str, args: &[Value]) -> Result<Value> {
        let event = NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
            code(b"ascr"),
            code(b"psbr"),
            Some(&NSAppleEventDescriptor::nullDescriptor()),
            -1, // kAutoGenerateReturnID
            0,  // kAnyTransactionID
        );
        event.setParamDescriptor_forKeyword(&NSAppleEventDescriptor::descriptorWithString(&NSString::from_str(handler)), code(b"snam"));
        event.setParamDescriptor_forKeyword(&pack(&Value::List(args.to_vec())), code(b"----"));

        let mut err: Option<Retained<NSDictionary<NSString, AnyObject>>> = None;
        // The generated binding claims a non-null return, but NSAppleScript returns nil on error.
        let result: Option<Retained<NSAppleEventDescriptor>> =
            unsafe { msg_send![&*self.0, executeAppleEvent: &*event, error: Some(&mut err)] };
        match result {
            Some(desc) => unpack(&desc),
            None => Err(error_from_info(err.as_deref())),
        }
    }
}

fn error_from_info(info: Option<&NSDictionary<NSString, AnyObject>>) -> Error {
    let Some(info) = info else { return Error::new("unknown AppleScript error") };
    let message = info
        .objectForKey(unsafe { NSAppleScriptErrorMessage })
        .and_then(|o| o.downcast::<NSString>().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "unknown AppleScript error".into());
    let number = info
        .objectForKey(unsafe { NSAppleScriptErrorNumber })
        .and_then(|o| o.downcast::<NSNumber>().ok())
        .map(|n| n.as_i64());
    Error { message, number }
}

fn pack(value: &Value) -> Retained<NSAppleEventDescriptor> {
    match value {
        Value::Missing => NSAppleEventDescriptor::descriptorWithTypeCode(MISSING_VALUE),
        Value::Bool(b) => NSAppleEventDescriptor::descriptorWithBoolean(*b as u8),
        Value::Int(i) => match i32::try_from(*i) {
            Ok(i) => NSAppleEventDescriptor::descriptorWithInt32(i),
            Err(_) => NSAppleEventDescriptor::descriptorWithDouble(*i as f64),
        },
        Value::Float(f) => NSAppleEventDescriptor::descriptorWithDouble(*f),
        Value::Text(s) => NSAppleEventDescriptor::descriptorWithString(&NSString::from_str(s)),
        Value::Date(d) => {
            let secs = (*d - mac_epoch()).num_seconds();
            NSAppleEventDescriptor::descriptorWithDescriptorType_data(TYPE_LDT, Some(&NSData::with_bytes(&secs.to_ne_bytes())))
                .expect("long date descriptor")
        }
        Value::List(items) => {
            let list = NSAppleEventDescriptor::listDescriptor();
            for item in items {
                // index 0 appends
                list.insertDescriptor_atIndex(&pack(item), 0);
            }
            list
        }
    }
}

fn unpack(desc: &NSAppleEventDescriptor) -> Result<Value> {
    Ok(match desc.descriptorType() {
        TYPE_NULL => Value::Missing,
        TYPE_TYPE if desc.typeCodeValue() == MISSING_VALUE => Value::Missing,
        TYPE_BOOL | TYPE_TRUE | TYPE_FALSE => Value::Bool(desc.booleanValue() != 0),
        TYPE_SHORT | TYPE_LONG => Value::Int(desc.int32Value().into()),
        TYPE_COMP => Value::Int(i64::from_ne_bytes(eight_bytes(desc)?)),
        TYPE_DOUBLE | TYPE_SINGLE => Value::Float(desc.doubleValue()),
        TYPE_UTXT | TYPE_UTF8 | TYPE_UT16 | TYPE_TEXT => {
            Value::Text(desc.stringValue().map(|s| s.to_string()).unwrap_or_default())
        }
        TYPE_LDT => Value::Date(mac_epoch() + TimeDelta::seconds(i64::from_ne_bytes(eight_bytes(desc)?))),
        TYPE_LIST => Value::List(
            (1..=desc.numberOfItems())
                .map(|i| desc.descriptorAtIndex(i).ok_or_else(|| Error::new("missing list item")).and_then(|d| unpack(&d)))
                .collect::<Result<_>>()?,
        ),
        other => {
            return Err(Error::new(format!(
                "unsupported AppleScript result type '{}'",
                String::from_utf8_lossy(&other.to_be_bytes())
            )));
        }
    })
}

fn eight_bytes(desc: &NSAppleEventDescriptor) -> Result<[u8; 8]> {
    desc.data().to_vec().try_into().map_err(|_| Error::new("malformed 8-byte descriptor"))
}

impl Value {
    pub fn is_missing(&self) -> bool {
        matches!(self, Value::Missing)
    }

    pub fn into_text(self) -> Result<String> {
        match self {
            Value::Text(s) => Ok(s),
            Value::Missing => Ok(String::new()),
            other => Err(Error::new(format!("expected text, got {other:?}"))),
        }
    }

    pub fn into_opt_text(self) -> Result<Option<String>> {
        match self {
            Value::Missing => Ok(None),
            v => v.into_text().map(Some),
        }
    }

    pub fn as_bool(&self) -> Result<bool> {
        match self {
            Value::Bool(b) => Ok(*b),
            other => Err(Error::new(format!("expected boolean, got {other:?}"))),
        }
    }

    pub fn as_i64(&self) -> Result<i64> {
        match self {
            Value::Int(i) => Ok(*i),
            Value::Float(f) => Ok(*f as i64),
            other => Err(Error::new(format!("expected integer, got {other:?}"))),
        }
    }

    pub fn as_opt_f64(&self) -> Result<Option<f64>> {
        match self {
            Value::Missing => Ok(None),
            Value::Int(i) => Ok(Some(*i as f64)),
            Value::Float(f) => Ok(Some(*f)),
            other => Err(Error::new(format!("expected number, got {other:?}"))),
        }
    }

    /// A list of text; a lone string becomes a one-item list, missing becomes empty.
    pub fn into_text_list(self) -> Result<Vec<String>> {
        match self {
            Value::List(items) => items.into_iter().map(Value::into_text).collect(),
            Value::Missing => Ok(vec![]),
            v => Ok(vec![v.into_text()?]),
        }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Text(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Text(s)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Self {
        Value::Int(i)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::List(v.into_iter().map(Into::into).collect())
    }
}
