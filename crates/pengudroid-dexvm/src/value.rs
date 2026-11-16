//! VM runtime values, object heap, arrays, and strings.

use std::collections::HashMap;

pub type ObjId = usize;

#[derive(Debug, Clone)]
pub enum Value {
    Void,
    Int(i32),
    Long(i64),
    Float(f32),
    Double(f64),
    Ref(Option<ObjId>),
}

impl Value {
    pub fn as_int(&self) -> Result<i32, String> {
        match self {
            Value::Int(v) => Ok(*v),
            other => Err(format!("expected Int, got {other:?}")),
        }
    }

    pub fn as_long(&self) -> Result<i64, String> {
        match self {
            Value::Long(v) => Ok(*v),
            other => Err(format!("expected Long, got {other:?}")),
        }
    }

    pub fn as_float(&self) -> Result<f32, String> {
        match self {
            Value::Float(v) => Ok(*v),
            Value::Int(v) => Ok(f32::from_bits(*v as u32)),
            other => Err(format!("expected Float, got {other:?}")),
        }
    }

    pub fn as_double(&self) -> Result<f64, String> {
        match self {
            Value::Double(v) => Ok(*v),
            Value::Long(v) => Ok(f64::from_bits(*v as u64)),
            other => Err(format!("expected Double, got {other:?}")),
        }
    }

    pub fn as_ref(&self) -> Option<ObjId> {
        match self {
            Value::Ref(r) => *r,
            Value::Int(0) => None,
            _ => None,
        }
    }

    pub fn is_zero(&self) -> bool {
        match self {
            Value::Int(0) => true,
            Value::Long(0) => true,
            Value::Ref(None) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum ArrayData {
    Int(Vec<i32>),
    Long(Vec<i64>),
    Byte(Vec<i8>),
    Boolean(Vec<bool>),
    Short(Vec<i16>),
    Char(Vec<u16>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    Object(Vec<Option<ObjId>>),
}

impl ArrayData {
    pub fn len(&self) -> usize {
        match self {
            ArrayData::Int(v) => v.len(),
            ArrayData::Long(v) => v.len(),
            ArrayData::Byte(v) => v.len(),
            ArrayData::Boolean(v) => v.len(),
            ArrayData::Short(v) => v.len(),
            ArrayData::Char(v) => v.len(),
            ArrayData::Float(v) => v.len(),
            ArrayData::Double(v) => v.len(),
            ArrayData::Object(v) => v.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum HeapObject {
    Instance {
        class_name: String,
        fields: HashMap<String, Value>,
    },
    StringObj(String),
    ClassObj(String),
    Array {
        type_desc: String,
        data: ArrayData,
    },
    List(Vec<Value>),
    Map(HashMap<String, Value>),
    StringBuilder(String),
}

impl HeapObject {
    pub fn class_name(&self) -> &str {
        match self {
            HeapObject::Instance { class_name, .. } => class_name,
            HeapObject::StringObj(_) => "Ljava/lang/String;",
            HeapObject::ClassObj(_) => "Ljava/lang/Class;",
            HeapObject::Array { type_desc, .. } => type_desc,
            HeapObject::List(_) => "Ljava/util/ArrayList;",
            HeapObject::Map(_) => "Ljava/util/HashMap;",
            HeapObject::StringBuilder(_) => "Ljava/lang/StringBuilder;",
        }
    }
}

#[derive(Debug, Default)]
pub struct Heap {
    objects: Vec<Option<HeapObject>>,
    interned_strings: HashMap<String, ObjId>,
    class_objects: HashMap<String, ObjId>,
}

impl Heap {
    pub fn new() -> Self {
        Self {
            // Index 0 is reserved for null.
            objects: vec![None],
            interned_strings: HashMap::new(),
            class_objects: HashMap::new(),
        }
    }

    pub fn alloc(&mut self, obj: HeapObject) -> ObjId {
        let id = self.objects.len();
        self.objects.push(Some(obj));
        id
    }

    pub fn get(&self, id: ObjId) -> Option<&HeapObject> {
        self.objects.get(id).and_then(|o| o.as_ref())
    }

    pub fn get_mut(&mut self, id: ObjId) -> Option<&mut HeapObject> {
        self.objects.get_mut(id).and_then(|o| o.as_mut())
    }

    pub fn intern_string(&mut self, s: &str) -> ObjId {
        if let Some(&id) = self.interned_strings.get(s) {
            return id;
        }
        let id = self.alloc(HeapObject::StringObj(s.to_string()));
        self.interned_strings.insert(s.to_string(), id);
        id
    }

    pub fn new_string(&mut self, s: String) -> ObjId {
        self.alloc(HeapObject::StringObj(s))
    }

    pub fn class_object(&mut self, class_desc: &str) -> ObjId {
        if let Some(&id) = self.class_objects.get(class_desc) {
            return id;
        }
        let id = self.alloc(HeapObject::ClassObj(class_desc.to_string()));
        self.class_objects.insert(class_desc.to_string(), id);
        id
    }

    pub fn new_instance(&mut self, class_name: &str) -> ObjId {
        self.alloc(HeapObject::Instance {
            class_name: class_name.to_string(),
            fields: HashMap::new(),
        })
    }

    pub fn new_array(&mut self, type_desc: &str, len: usize) -> Result<ObjId, String> {
        let elem = type_desc.strip_prefix('[').unwrap_or("Ljava/lang/Object;");
        let data = match elem {
            "I" => ArrayData::Int(vec![0; len]),
            "J" => ArrayData::Long(vec![0; len]),
            "B" => ArrayData::Byte(vec![0; len]),
            "Z" => ArrayData::Boolean(vec![false; len]),
            "S" => ArrayData::Short(vec![0; len]),
            "C" => ArrayData::Char(vec![0; len]),
            "F" => ArrayData::Float(vec![0.0; len]),
            "D" => ArrayData::Double(vec![0.0; len]),
            _ => ArrayData::Object(vec![None; len]),
        };
        Ok(self.alloc(HeapObject::Array {
            type_desc: type_desc.to_string(),
            data,
        }))
    }

    pub fn as_str(&self, id: ObjId) -> Option<&str> {
        match self.get(id) {
            Some(HeapObject::StringObj(s)) => Some(s.as_str()),
            _ => None,
        }
    }
}
