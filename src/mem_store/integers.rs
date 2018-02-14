use bit_vec::BitVec;
use mem_store::ingest::RawVal;
use mem_store::column::{ColumnData, ColumnCodec};
use mem_store::point_codec::PointCodec;
use heapsize::HeapSizeOf;
use std::{u8, u16, u32, i64};
use num::traits::NumCast;
use engine::types::Type;
use engine::typed_vec::TypedVec;


pub struct IntegerColumn {
    values: Vec<i64>,
}

impl IntegerColumn {
    // TODO(clemens): do not subtract offset if it does not change encoding size
    pub fn new(mut values: Vec<i64>, min: i64, max: i64) -> Box<ColumnData> {
        if max - min <= u8::MAX as i64 {
            Box::new(IntegerOffsetColumn::<u8>::new(values, min))
        } else if max - min <= u16::MAX as i64 {
            Box::new(IntegerOffsetColumn::<u16>::new(values, min))
        } else if max - min <= u32::MAX as i64 {
            Box::new(IntegerOffsetColumn::<u32>::new(values, min))
        } else {
            values.shrink_to_fit();
            Box::new(IntegerColumn { values: values })
        }
    }
}

impl ColumnData for IntegerColumn {
    fn collect_decoded(&self) -> TypedVec {
        TypedVec::Integer(self.values.clone())
    }

    fn filter_decode<'a>(&'a self, filter: &BitVec) -> TypedVec {
        let mut results = Vec::with_capacity(self.values.len());
        for (i, select) in filter.iter().enumerate() {
            if select {
                results.push(self.values[i]);
            }
        }
        TypedVec::Integer(results)
    }

    fn decoded_type(&self) -> Type { Type::I64 }
}


struct IntegerOffsetColumn<T: IntLike> {
    values: Vec<T>,
    offset: i64,
}

impl<T: IntLike> IntegerOffsetColumn<T> {
    fn new(values: Vec<i64>, offset: i64) -> IntegerOffsetColumn<T> {
        let mut encoded_vals = Vec::with_capacity(values.len());
        for v in values {
            encoded_vals.push(T::from(v - offset).unwrap());
        }
        IntegerOffsetColumn {
            values: encoded_vals,
            offset: offset,
        }
    }
}

impl<T: IntLike> ColumnData for IntegerOffsetColumn<T> {
    fn collect_decoded(&self) -> TypedVec {
        self.decode(&self.values)
    }

    fn filter_decode(&self, filter: &BitVec) -> TypedVec {
        let mut result = Vec::with_capacity(self.values.len());
        for (i, select) in filter.iter().enumerate() {
            if select {
                result.push(self.values[i].to_i64().unwrap() + self.offset);
            }
        }
        TypedVec::Integer(result)
    }

    fn decoded_type(&self) -> Type { Type::I64 }

    fn to_codec(&self) -> Option<&ColumnCodec> { Some(self as &ColumnCodec) }
}

impl<T: IntLike> PointCodec<T> for IntegerOffsetColumn<T> {
    fn decode(&self, data: &[T]) -> TypedVec {
        let mut result = Vec::with_capacity(self.values.len());
        for value in data {
            result.push(value.to_i64().unwrap() + self.offset);
        }
        TypedVec::Integer(result)
    }

    fn to_raw(&self, elem: T) -> RawVal {
        RawVal::Int(elem.to_i64().unwrap() + self.offset)
    }
}

impl<T: IntLike> ColumnCodec for IntegerOffsetColumn<T> {
    fn get_encoded(&self) -> TypedVec {
        T::borrowed_typed_vec(&self.values, self as &PointCodec<T>)
    }

    fn filter_encoded(&self, filter: &BitVec) -> TypedVec {
        let filtered_values = self.values.iter().zip(filter.iter())
            .filter(|&(_, select)| select)
            .map(|(i, _)| *i)
            .collect();
        T::typed_vec(filtered_values, self as &PointCodec<T>)
    }

    fn encoded_type(&self) -> Type { T::t() }
    fn ref_encoded_type(&self) -> Type { T::t_ref() }
}

impl HeapSizeOf for IntegerColumn {
    fn heap_size_of_children(&self) -> usize {
        self.values.heap_size_of_children()
    }
}

trait IntLike: NumCast + HeapSizeOf + Copy + Send + Sync {
    fn borrowed_typed_vec<'a>(values: &'a [Self], codec: &'a PointCodec<Self>) -> TypedVec<'a>;
    fn typed_vec<'a>(values: Vec<Self>, codec: &'a PointCodec<Self>) -> TypedVec<'a>;
    fn t() -> Type;
    fn t_ref() -> Type;
}

impl IntLike for u8 {
    fn borrowed_typed_vec<'a>(values: &'a [Self], codec: &'a PointCodec<Self>) -> TypedVec<'a> {
        TypedVec::BorrowedEncodedU8(values, codec)
    }

    fn typed_vec<'a>(values: Vec<Self>, codec: &'a PointCodec<Self>) -> TypedVec<'a> {
        TypedVec::EncodedU8(values, codec)
    }

    fn t() -> Type { Type::U8 }
    fn t_ref() -> Type { Type::RefU8 }
}

impl IntLike for u16 {
    fn borrowed_typed_vec<'a>(values: &'a [Self], codec: &'a PointCodec<Self>) -> TypedVec<'a> {
        TypedVec::BorrowedEncodedU16(values, codec)
    }

    fn typed_vec<'a>(values: Vec<Self>, codec: &'a PointCodec<Self>) -> TypedVec<'a> {
        TypedVec::EncodedU16(values, codec)
    }

    fn t() -> Type { Type::U8 }
    fn t_ref() -> Type { Type::RefU8 }
}

impl IntLike for u32 {
    fn borrowed_typed_vec<'a>(values: &'a [Self], codec: &'a PointCodec<Self>) -> TypedVec<'a> {
        TypedVec::BorrowedEncodedU32(values, codec)
    }

    fn typed_vec<'a>(values: Vec<Self>, codec: &'a PointCodec<Self>) -> TypedVec<'a> {
        TypedVec::EncodedU32(values, codec)
    }

    fn t() -> Type { Type::U8 }
    fn t_ref() -> Type { Type::RefU8 }
}

impl<T: IntLike> HeapSizeOf for IntegerOffsetColumn<T> {
    fn heap_size_of_children(&self) -> usize {
        self.values.heap_size_of_children()
    }
}
