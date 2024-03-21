use std::u8;

pub trait ToCbor {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8>
    where
        Self: Sized;
}

fn write_uint_minor(major: u8, val: u64) -> Vec<u8> {
    if val < 24 {
        vec![(major << 5) | (val as u8)]
    } else if val <= u8::MAX as u64 {
        vec![(major << 5) | 24u8, val as u8]
    } else if val <= u16::MAX as u64 {
        vec![(major << 5) | 25u8, (val >> 8) as u8, (val & 0xFF) as u8]
    } else if val <= u32::MAX as u64 {
        vec![
            (major << 5) | 26u8,
            (val >> 24) as u8,
            (val >> 16) as u8,
            (val >> 8) as u8,
            val as u8,
        ]
    } else {
        vec![
            (major << 5) | 27u8,
            (val >> 56) as u8,
            (val >> 48) as u8,
            (val >> 40) as u8,
            (val >> 32) as u8,
            (val >> 24) as u8,
            (val >> 16) as u8,
            (val >> 8) as u8,
            val as u8,
        ]
    }
}

pub fn write_with_tags<T>(value: T, tags: &[u64]) -> Vec<u8>
where
    T: ToCbor,
{
    value.to_cbor(tags)
}

pub fn write<T>(value: T) -> Vec<u8>
where
    T: ToCbor,
{
    value.to_cbor(&[])
}

fn write_tags(tags: &[u64]) -> Vec<u8> {
    let mut v = Vec::new();
    for tag in tags {
        v.extend(write_uint_minor(6, *tag))
    }
    v
}

impl ToCbor for u64 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = write_tags(tags);
        v.extend(write_uint_minor(0, self));
        v
    }
}

impl ToCbor for u32 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = write_tags(tags);
        v.extend(write_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for u16 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = write_tags(tags);
        v.extend(write_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for u8 {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = write_tags(tags);
        v.extend(write_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for usize {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = write_tags(tags);
        v.extend(write_uint_minor(0, self as u64));
        v
    }
}

impl ToCbor for &str {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = write_tags(tags);
        v.extend(write_uint_minor(3, self.len() as u64));
        v.extend(self.as_bytes());
        v
    }
}

impl ToCbor for &Vec<Vec<u8>> {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        let mut v = write_tags(tags);
        v.extend(write_uint_minor(6, self.len() as u64));
        for i in self {
            v.extend(i);
        }
        v
    }
}
