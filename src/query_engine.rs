use std::iter::Iterator;
use std::rc::Rc;
use std::collections::HashMap;
use std::collections::HashSet;
use seahash::SeaHasher;
use std::hash::BuildHasherDefault;
use time::precise_time_ns;
use std::ops::Add;

use value::Val;
use expression::*;
use aggregator::*;
use limit::*;
use util::fmt_table;
use mem_store::column::Column;
use mem_store::column::ColIter;
use mem_store::batch::Batch;
use mem_store::ingest::RawVal;


#[derive(Debug)]
pub struct Query {
    pub select: Vec<Expr>,
    pub table: String,
    pub filter: Expr,
    pub aggregate: Vec<(Aggregator, Expr)>,
    pub order_by: Option<Expr>,
    pub limit: LimitClause,
}

pub struct CompiledQuery<'a> {
    subqueries: Vec<CompiledSingleBatchQuery<'a>>,
    output_colnames: Vec<Rc<String>>,
    aggregate: Vec<Aggregator>,
    compiled_order_by: Option<Expr>,
    limit: LimitClause,
}

struct CompiledSingleBatchQuery<'a> {
    select: Vec<Expr>,
    filter: Expr,
    aggregate: Vec<(Aggregator, Expr)>,
    coliter: Vec<ColIter<'a>>,
}

pub struct QueryResult {
    pub colnames: Vec<Rc<String>>,
    pub rows: Vec<Vec<RawVal>>,
    pub stats: QueryStats,
}

struct SelectSubqueryResult<'a> {
    rows: Vec<Vec<Val<'a>>>,
    stats: QueryStats,
}

#[derive(Debug)]
struct AggregateSubqueryResult<'a> {
    groups: HashMap<Vec<Val<'a>>, Vec<Val<'a>>, BuildSeaHasher>,
    stats: QueryStats,
}

#[derive(Debug)]
pub struct QueryStats {
    pub runtime_ns: u64,
    pub rows_scanned: u64,
}

type BuildSeaHasher = BuildHasherDefault<SeaHasher>;

impl QueryStats {
    fn new(runtime_ns: u64, rows_scanned: u64) -> QueryStats {
        QueryStats {
            runtime_ns: runtime_ns,
            rows_scanned: rows_scanned,
        }
    }

    fn combine(&self, other: &QueryStats) -> QueryStats {
        QueryStats::new(self.runtime_ns + other.runtime_ns,
                        self.rows_scanned + other.rows_scanned)
    }
}

impl Add for QueryStats {
    type Output = QueryStats;

    fn add(self, other: QueryStats) -> QueryStats {
        QueryStats {
            runtime_ns: self.runtime_ns + other.runtime_ns,
            rows_scanned: self.rows_scanned + other.rows_scanned,
        }
    }
}


impl<'a> CompiledQuery<'a> {
    pub fn run(&mut self) -> QueryResult {
        let colnames = self.output_colnames.clone();
        let (mut result_rows, stats) = if self.aggregate.len() == 0 {
            let mut combined_results = SelectSubqueryResult {
                rows: Vec::new(),
                stats: QueryStats::new(0, 0),
            };
            for single_batch_query in &mut self.subqueries {
                let batch_result = single_batch_query.run_select_query();
                combined_results.rows.extend(batch_result.rows);
                combined_results.stats = combined_results.stats.combine(&batch_result.stats);
                if self.compiled_order_by.is_none() && self.limit.offset == 0 && (self.limit.limit as usize) < combined_results.rows.len() {
                    break;
                }
            }
            (combined_results.rows, combined_results.stats)
        } else {
            let mut combined_results = AggregateSubqueryResult {
                groups: HashMap::default(),
                stats: QueryStats::new(0, 0),
            };
            for single_batch_query in &mut self.subqueries {
                let batch_result = single_batch_query.run_aggregation_query();
                combined_results.stats = combined_results.stats.combine(&batch_result.stats);
                for (group, accumulator1) in batch_result.groups.into_iter() {
                    if let Some(mut accumulator2) = combined_results.groups.get_mut(&group) {
                        for (i, agg_func) in self.aggregate.iter().enumerate() {
                            accumulator2[i] = agg_func.combine(&accumulator1[i], &accumulator2[i]);
                        }
                    }
                    if !combined_results.groups.contains_key(&group) {
                        combined_results.groups.insert(group, accumulator1);
                    }
                }
            }

            let mut result: Vec<Vec<Val>> = Vec::new();
            for (mut group, aggregate) in combined_results.groups {
                group.extend(aggregate);
                result.push(group);
            }
            (result, combined_results.stats)
        };

        if let Some(ref order_by_expr) = self.compiled_order_by {
            result_rows.sort_by_key(|record| order_by_expr.eval(record));
        }

        let limited_result_rows = result_rows.into_iter()
            .skip(self.limit.offset as usize)
            .take(self.limit.limit as usize)
            .map(|row| row.iter().map(RawVal::from).collect())
            .collect();

        QueryResult {
            colnames: colnames,
            rows: limited_result_rows,
            stats: stats,
        }
    }
}

impl<'a> CompiledSingleBatchQuery<'a> {
    fn run_select_query(&mut self) -> SelectSubqueryResult {
        let mut result = Vec::new();
        let mut record = Vec::with_capacity(self.coliter.len());
        let start_time_ns = precise_time_ns();
        let mut rows_touched = 0;
        if self.coliter.len() == 0 {
            return SelectSubqueryResult {
                rows: Vec::new(),
                stats: QueryStats::new(0, 0),
            };
        }
        loop {
            record.clear();
            for i in 0..self.coliter.len() {
                match self.coliter[i].next() {
                    Some(item) => record.push(item),
                    None => {
                        return SelectSubqueryResult {
                            rows: result,
                            stats: QueryStats::new(precise_time_ns() - start_time_ns, rows_touched),
                        }
                    }
                }
            }
            if self.filter.eval(&record) == Val::Bool(true) {
                result.push(self.select.iter().map(|expr| expr.eval(&record)).collect());
            }
            rows_touched += 1
        }
    }

    fn run_aggregation_query(&mut self) -> AggregateSubqueryResult {
        let mut groups = HashMap::<Vec<Val>, Vec<Val>, BuildSeaHasher>::default();
        let mut record = Vec::with_capacity(self.coliter.len());
        let start_time_ns = precise_time_ns();
        let mut rows_touched = 0;
        'outer: loop {
            record.clear();
            for i in 0..self.coliter.len() {
                match self.coliter[i].next() {
                    Some(item) => record.push(item),
                    None => break 'outer,
                }
            }
            if self.filter.eval(&record) == Val::Bool(true) {
                let group: Vec<Val> =
                    self.select.iter().map(|expr| expr.eval(&record)).collect();
                let accumulator = groups.entry(group)
                    .or_insert(self.aggregate.iter().map(|x| x.0.zero()).collect());
                for (i, &(ref agg_func, ref expr)) in self.aggregate.iter().enumerate() {
                    accumulator[i] = agg_func.reduce(&accumulator[i], &expr.eval(&record));
                }
            }
            if self.coliter.len() == 0 {
                break;
            }
            rows_touched += 1;
        }
        AggregateSubqueryResult {
            groups: groups,
            stats: QueryStats::new(precise_time_ns() - start_time_ns, rows_touched),
        }
    }
}

impl Query {
    pub fn compile<'a>(&mut self, source: &'a Vec<Batch>) -> CompiledQuery<'a> {
        if self.is_select_star() {
            self.select = find_all_cols(source).into_iter().map(Expr::ColName).collect();
        }

        let subqueries = source.iter().map(|batch| self.compile_for_batch(batch)).collect();
        let limit = self.limit.clone();

        // Compile the order_by
        let output_colnames = self.result_column_names();
        let mut output_colmap = HashMap::new();
        for (i, output_colname) in output_colnames.iter().enumerate() {
            output_colmap.insert(output_colname.to_string(), i);
        }

        // Insert a placeholder sorter if ordering isn't specified
        let compiled_order_by = match self.order_by {
            Some(ref order_by) => Some(order_by.compile(&output_colmap)),
            None => None,
        };

        CompiledQuery {
            subqueries: subqueries,
            output_colnames: output_colnames,
            aggregate: self.aggregate.iter().map(|&(aggregate, _)| aggregate).collect(),
            compiled_order_by: compiled_order_by,
            limit: limit,
        }
    }

    fn compile_for_batch<'a>(&self, source: &'a Batch) -> CompiledSingleBatchQuery<'a> {
        let referenced_cols = self.find_referenced_cols();
        let efficient_source: Vec<&Column> = source.cols
            .iter()
            .filter(|col| referenced_cols.contains(&col.get_name().to_string()))
            .collect();
        let coliter = efficient_source.iter().map(|col| col.iter()).collect();
        let column_indices = create_colname_map(&efficient_source);
        let compiled_selects = self.select.iter().map(|expr| expr.compile(&column_indices)).collect();
        let compiled_filter = self.filter.compile(&column_indices);
        let compiled_aggregate = self.aggregate
            .iter()
            .map(|&(agg, ref expr)| (agg, expr.compile(&column_indices)))
            .collect();
        CompiledSingleBatchQuery {
            select: compiled_selects,
            filter: compiled_filter,
            aggregate: compiled_aggregate,
            coliter: coliter,
        }
    }

    fn is_select_star(&self) -> bool {
        if self.select.len() == 1 {
            match self.select[0] {
                Expr::ColName(ref colname) if **colname == "*".to_string() => true,
                _ => false,
            }
        } else {
            false
        }
    }
    fn result_column_names(&self) -> Vec<Rc<String>> {
        let mut anon_columns = -1;
        let select_cols = self.select
            .iter()
            .map(|expr| match expr {
                &Expr::ColName(ref name) => name.clone(),
                _ => {
                    anon_columns += 1;
                    Rc::new(format!("col_{}", anon_columns))
                }
            });
        let mut anon_aggregates = -1;
        let aggregate_cols = self.aggregate
            .iter()
            .map(|&(agg, _)| {
                anon_aggregates += 1;
                match agg {
                    Aggregator::Count => Rc::new(format!("count_{}", anon_aggregates)),
                    Aggregator::Sum => Rc::new(format!("sum_{}", anon_aggregates)),
                }
            });

        select_cols.chain(aggregate_cols).collect()
    }


    fn find_referenced_cols(&self) -> HashSet<Rc<String>> {
        let mut colnames = HashSet::new();
        for expr in self.select.iter() {
            expr.add_colnames(&mut colnames);
        }
        self.filter.add_colnames(&mut colnames);
        for &(_, ref expr) in self.aggregate.iter() {
            expr.add_colnames(&mut colnames);
        }
        colnames
    }
}

fn find_all_cols(source: &Vec<Batch>) -> Vec<Rc<String>> {
    let mut cols = HashSet::new();
    for batch in source {
        for column in &batch.cols {
            cols.insert(column.get_name().to_string());
        }
    }

    cols.into_iter().map(Rc::new).collect()
}

fn create_colname_map(source: &Vec<&Column>) -> HashMap<String, usize> {
    let mut mem_store = HashMap::new();
    for (i, col) in source.iter().enumerate() {
        mem_store.insert(col.get_name().to_string(), i as usize);
    }
    mem_store
}

pub fn print_query_result(results: &QueryResult) {
    let rt = results.stats.runtime_ns;
    let fmt_time = if rt < 10_000 {
        format!("{}ns", rt)
    } else if rt < 10_000_000 {
        format!("{}μs", rt / 1000)
    } else if rt < 10_000_000_000 {
        format!("{}ms", rt / 1_000_000)
    } else {
        format!("{}s", rt / 1_000_000_000)
    };

    println!("Scanned {} rows in {} ({}ns per row)!\n",
             results.stats.rows_scanned,
             fmt_time,
             rt.checked_div(results.stats.rows_scanned).unwrap_or(0));
    println!("{}\n", format_results(&results.colnames, &results.rows));
}

fn format_results(colnames: &Vec<Rc<String>>, rows: &Vec<Vec<RawVal>>) -> String {
    let strcolnames: Vec<&str> = colnames.iter().map(|ref s| s.clone() as &str).collect();
    let formattedrows: Vec<Vec<String>> = rows.iter()
        .map(|row| {
            row.iter()
                .map(|val| format!("{}", val))
                .collect()
        })
        .collect();
    let strrows =
        formattedrows.iter().map(|row| row.iter().map(|val| val as &str).collect()).collect();

    fmt_table(&strcolnames, &strrows)
}
