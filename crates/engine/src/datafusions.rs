use std::{collections::HashSet, lazy::SyncLazy, sync::Arc};

use arrow::{
    array::{
        ArrayData, ArrayRef, Date16Array, DecimalArray, Float32Array,
        Float64Array, GenericStringArray, Int8Array, Int16Array, Int32Array,
        Int64Array, Timestamp32Array, UInt8Array, UInt16Array, UInt32Array,
        UInt64Array,
    },
    buffer::Buffer,
    datatypes::{DataType, Field, Schema},
    ffi::FFI_ArrowArray,
    record_batch::RecordBatch,
};
use datafusion::{
    datasource::MemTable, error::Result, prelude::ExecutionContext,
};
use lang::parse::TablesContext;
use meta::{
    store::{
        parts::{CoPaInfo, PartStore},
        sys::MetaStore,
    },
    types::{BqlType, Id},
};
use tokio::runtime::{self, Runtime};

use crate::{
    errs::{EngineError, EngineResult},
    types::QueryState,
};

static TOKIO_RT: SyncLazy<Runtime> =
    SyncLazy::new(|| runtime::Builder::new_multi_thread().build().unwrap());

fn btype_to_arrow_type(typ: BqlType) -> EngineResult<DataType> {
    match typ {
        BqlType::UInt(bits) if bits == 8 => Ok(DataType::UInt8),
        BqlType::UInt(bits) if bits == 16 => Ok(DataType::UInt16),
        BqlType::UInt(bits) if bits == 32 => Ok(DataType::UInt32),
        BqlType::UInt(bits) if bits == 64 => Ok(DataType::UInt64),
        BqlType::Int(bits) if bits == 8 => Ok(DataType::Int8),
        BqlType::Int(bits) if bits == 16 => Ok(DataType::Int16),
        BqlType::Int(bits) if bits == 32 => Ok(DataType::Int32),
        BqlType::Int(bits) if bits == 64 => Ok(DataType::Int64),
        BqlType::Float(bits) if bits == 16 => Ok(DataType::Float16),
        BqlType::Float(bits) if bits == 32 => Ok(DataType::Float32),
        BqlType::Float(bits) if bits == 64 => Ok(DataType::Float64),
        BqlType::DateTime => Ok(DataType::Timestamp32(None)),
        BqlType::Date => Ok(DataType::Date16),
        BqlType::Decimal(p, s) => Ok(DataType::Decimal(p as usize, s as usize)),
        BqlType::String => Ok(DataType::LargeUtf8),
        BqlType::LowCardinalityString => Ok(DataType::UInt32),
        _ => Err(EngineError::UnsupportedBqlType),
    }
}

pub(crate) fn run(
    ms: &MetaStore,
    ps: &PartStore,
    current_db: &str,
    raw_query: &str,
    _query_id: &str,
    tctx: TablesContext,
    qs: &mut QueryState,
) -> EngineResult<Vec<RecordBatch>> {
    // let t = Instant::now();
    let mut ctx = ExecutionContext::new();
    let tabs = tctx.tabs;
    let cols = tctx.cols;
    for tab in tabs {
        let qn1 = [current_db, &tab].join(".");
        let qtn = if tab.contains('.') { &tab } else { &qn1 };
        let tid = ms.tid_by_qname(&qtn).ok_or(EngineError::TableNotExist)?;
        // *cid, ci.data_type
        let mut cis = Vec::new();
        let mut fields = Vec::new();
        if cols.len() != 0 {
            for cn in &cols {
                let qcn = if cn.contains('.') {
                    // ms.cid_by_qname(&cn).ok_or(EngineError::ColumnNotExist)?
                    //FIXME for t.c, not work for db.t.c
                    [current_db, cn].join(".")
                } else {
                    [qtn, cn.as_str()].join(".")
                };
                if qcn.contains(qtn) {
                    if let Some(cid) = ms.cid_by_qname(&qcn) {
                        if let Some(ci) = ms.get_column_info(cid)? {
                            cis.push((cid, ci.data_type));
                            fields.push(Field::new(
                                cn,
                                btype_to_arrow_type(ci.data_type)?,
                                false,
                            ));
                        } else {
                            return Err(EngineError::ColumnInfoNotExist);
                        }
                    }
                }
            }
        } else {
            if tctx.has_count_all {
                log::debug!("current_db: {}, cn: {}", current_db, &tab);
                let cs = ms.get_columns_by_qtn(qtn)?;
                let (cn, cid, ci) = &cs[0];
                cis.push((*cid, ci.data_type));
                fields.push(Field::new(
                    cn.as_str(),
                    btype_to_arrow_type(ci.data_type)?,
                    false,
                ));
            } else if tctx.has_select_all {
                let cs = ms.get_columns_by_qtn(qtn)?;
                for (cn, cid, ci) in cs {
                    cis.push((cid, ci.data_type));
                    fields.push(Field::new(
                        cn.as_str(),
                        btype_to_arrow_type(ci.data_type)?,
                        false,
                    ));
                }
            } else {
                return Err(EngineError::UnsupportedQuery);
            }
        }
        //log::debug!("[df][Schema] - fields: {:?}", fields);
        let schema = Arc::new(Schema::new(fields));
        let copasss = &mut qs.copasss;
        let mut copass = Vec::new();
        ps.fill_copainfos_int_by_ptk_range(
            &mut copass,
            tid,
            &cis,
            0,
            u64::MAX,
        )?;
        if copass.len() == 0 {
            return Err(EngineError::UnexpectedDataLoadingError);
        }
        log::debug!(
            "got {} copas, with {} copa per copas",
            copass.len(),
            copass[0].len(),
        );

        setup_tables(tab.as_str(), schema, &mut ctx, &cis, &copass)?;

        copasss.push(copass);
    }
    // log::info!("query setup runtime(ms): {}", t.elapsed().as_millis());

    //FIXME copa prunning

    let df = ctx.sql(raw_query)?;
    let res: Result<Vec<RecordBatch>> = TOKIO_RT.block_on(async move {
        let result = df.collect().await?;
        // arrow::util::pretty::print_batches(&result)?;
        Ok(result)
    });
    Ok(res?)
}

fn setup_tables(
    tabname: &str,
    schema: Arc<Schema>,
    ctx: &mut ExecutionContext,
    cis: &Vec<(Id, BqlType)>,
    copass: &Vec<Vec<CoPaInfo>>,
) -> EngineResult<()> {
    let nc = copass.len();
    let np = copass[0].len();
    let mut batches = Vec::with_capacity(np);
    for i in 0..np {
        let mut cols: Vec<ArrayRef> = Vec::with_capacity(nc);
        for j in 0..nc {
            let copa = &copass[j];
            let cpi = &copa[i];
            let typ = btype_to_arrow_type(cis[j].1)?;
            let data = gen_arrow_arraydata(cpi, &typ)?;
            match typ {
                DataType::Int8 => {
                    cols.push(Arc::new(Int8Array::from(data)));
                }
                DataType::Int16 => {
                    cols.push(Arc::new(Int16Array::from(data)));
                }
                DataType::Int32 => {
                    cols.push(Arc::new(Int32Array::from(data)));
                }
                DataType::Int64 => {
                    cols.push(Arc::new(Int64Array::from(data)));
                }
                DataType::UInt8 => {
                    cols.push(Arc::new(UInt8Array::from(data)));
                }
                DataType::UInt16 => {
                    cols.push(Arc::new(UInt16Array::from(data)));
                }
                DataType::UInt32 => {
                    cols.push(Arc::new(UInt32Array::from(data)));
                }
                DataType::UInt64 => {
                    cols.push(Arc::new(UInt64Array::from(data)));
                }
                // TODO: arrow::array does not have Float16Array
                // DataType::Float16 => {
                //     cols.push(Arc::new(Float16Array::from(data)));
                // }
                DataType::Float32 => {
                    cols.push(Arc::new(Float32Array::from(data)));
                }
                DataType::Float64 => {
                    cols.push(Arc::new(Float64Array::from(data)));
                }
                DataType::Timestamp32(_) => {
                    cols.push(Arc::new(Timestamp32Array::from(data)));
                }
                DataType::Date16 => {
                    cols.push(Arc::new(Date16Array::from(data)));
                }
                DataType::Decimal(_, _) => {
                    cols.push(Arc::new(DecimalArray::from(data)));
                }
                DataType::LargeUtf8 => {
                    cols.push(Arc::new(GenericStringArray::<i64>::from(data)));
                }
                // DataType::Null => {}
                // DataType::Boolean => {}
                // DataType::Timestamp(_, _) => {}
                // DataType::Date64 => {}
                // DataType::Time32(_) => {}
                // DataType::Time64(_) => {}
                // DataType::Duration(_) => {}
                // DataType::Interval(_) => {}
                // DataType::Binary => {}
                // DataType::FixedSizeBinary(_) => {}
                // DataType::LargeBinary => {}
                // DataType::Utf8 => {}
                // DataType::List(_) => {}
                // DataType::FixedSizeList(_, _) => {}
                // DataType::LargeList(_) => {}
                // DataType::Struct(_) => {}
                // DataType::Union(_) => {}
                // DataType::Dictionary(_, _) => {}
                _ => return Err(EngineError::UnsupportedBqlType),
            }
        }
        let batch = RecordBatch::try_new(schema.clone(), cols)?;
        batches.push(batch);
    }
    // println!("batches.len: {}", batches.len());
    ctx.register_table(
        tabname,
        Arc::new(MemTable::try_new(schema.clone(), vec![batches])?),
    )?;

    Ok(())
}

fn gen_arrow_arraydata(
    cpi: &CoPaInfo,
    typ: &DataType,
) -> EngineResult<ArrayData> {
    let dummy = Arc::new(FFI_ArrowArray::empty());
    let buf = unsafe {
        let ptr = std::ptr::NonNull::new(cpi.addr as *mut u8)
            .ok_or(EngineError::UnwrapOptionError)?;
        Buffer::from_unowned(ptr, cpi.len_in_bytes, dummy)
    };
    let data = if matches!(typ, DataType::LargeUtf8) {
        let dummy_om = Arc::new(FFI_ArrowArray::empty());
        let buf_om = unsafe {
            let ptr = std::ptr::NonNull::new(cpi.addr_om as *mut u8)
                .ok_or(EngineError::UnwrapOptionError)?;
            Buffer::from_unowned(
                ptr,
                CoPaInfo::len_in_bytes_om(cpi.size),
                dummy_om,
            )
        };
        ArrayData::builder(typ.clone())
            .len(cpi.size)
            .add_buffer(buf_om)
            .add_buffer(buf)
            .build()
    } else {
        ArrayData::builder(typ.clone())
            .len(cpi.size)
            .add_buffer(buf)
            .build()
    };
    Ok(data)
}

#[cfg(test)]
mod unit_tests {
    #[test]
    fn test() {}
}
