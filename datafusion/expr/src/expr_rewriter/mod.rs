// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Expression rewriter

use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt::Debug;
use std::sync::Arc;

use crate::expr::{Alias, Sort, Unnest};
use crate::logical_plan::Projection;
use crate::{Expr, ExprSchemable, LogicalPlan, LogicalPlanBuilder};

use datafusion_common::config::ConfigOptions;
use datafusion_common::tree_node::{Transformed, TransformedResult, TreeNode};
use datafusion_common::TableReference;
use datafusion_common::{Column, DFSchema, Result};

mod order_by;
pub use order_by::rewrite_sort_cols_by_aggs;

/// Trait for rewriting [`Expr`]s into function calls.
///
/// This trait is used with `FunctionRegistry::register_function_rewrite` to
/// to evaluating `Expr`s using functions that may not be built in to DataFusion
///
/// For example, concatenating arrays `a || b` is represented as
/// `Operator::ArrowAt`, but can be implemented by calling a function
/// `array_concat` from the `functions-nested` crate.
// This is not used in datafusion internally, but it is still helpful for downstream project so don't remove it.
pub trait FunctionRewrite: Debug {
    /// Return a human readable name for this rewrite
    fn name(&self) -> &str;

    /// Potentially rewrite `expr` to some other expression
    ///
    /// Note that recursion is handled by the caller -- this method should only
    /// handle `expr`, not recurse to its children.
    fn rewrite(
        &self,
        expr: Expr,
        schema: &DFSchema,
        config: &ConfigOptions,
    ) -> Result<Transformed<Expr>>;
}

/// Recursively call `LogicalPlanBuilder::normalize` on all [`Column`] expressions
/// in the `expr` expression tree.
pub fn normalize_col(expr: Expr, plan: &LogicalPlan) -> Result<Expr> {
    expr.transform(|expr| {
        Ok({
            if let Expr::Column(c) = expr {
                let col = LogicalPlanBuilder::normalize(plan, c)?;
                Transformed::yes(Expr::Column(col))
            } else {
                Transformed::no(expr)
            }
        })
    })
    .data()
}

/// See [`Column::normalize_with_schemas_and_ambiguity_check`] for usage
pub fn normalize_col_with_schemas_and_ambiguity_check(
    expr: Expr,
    schemas: &[&[&DFSchema]],
    using_columns: &[HashSet<Column>],
) -> Result<Expr> {
    // Normalize column inside Unnest
    if let Expr::Unnest(Unnest { expr }) = expr {
        let e = normalize_col_with_schemas_and_ambiguity_check(
            expr.as_ref().clone(),
            schemas,
            using_columns,
        )?;
        return Ok(Expr::Unnest(Unnest { expr: Box::new(e) }));
    }

    expr.transform(|expr| {
        Ok({
            if let Expr::Column(c) = expr {
                let col =
                    c.normalize_with_schemas_and_ambiguity_check(schemas, using_columns)?;
                Transformed::yes(Expr::Column(col))
            } else {
                Transformed::no(expr)
            }
        })
    })
    .data()
}

/// Recursively normalize all [`Column`] expressions in a list of expression trees
pub fn normalize_cols(
    exprs: impl IntoIterator<Item = impl Into<Expr>>,
    plan: &LogicalPlan,
) -> Result<Vec<Expr>> {
    exprs
        .into_iter()
        .map(|e| normalize_col(e.into(), plan))
        .collect()
}

pub fn normalize_sorts(
    sorts: impl IntoIterator<Item = impl Into<Sort>>,
    plan: &LogicalPlan,
) -> Result<Vec<Sort>> {
    sorts
        .into_iter()
        .map(|e| {
            let sort = e.into();
            normalize_col(sort.expr, plan)
                .map(|expr| Sort::new(expr, sort.asc, sort.nulls_first))
        })
        .collect()
}

/// Recursively replace all [`Column`] expressions in a given expression tree with
/// `Column` expressions provided by the hash map argument.
pub fn replace_col(expr: Expr, replace_map: &HashMap<&Column, &Column>) -> Result<Expr> {
    expr.transform(|expr| {
        Ok({
            if let Expr::Column(c) = &expr {
                match replace_map.get(c) {
                    Some(new_c) => Transformed::yes(Expr::Column((*new_c).to_owned())),
                    None => Transformed::no(expr),
                }
            } else {
                Transformed::no(expr)
            }
        })
    })
    .data()
}

/// Recursively 'unnormalize' (remove all qualifiers) from an
/// expression tree.
///
/// For example, if there were expressions like `foo.bar` this would
/// rewrite it to just `bar`.
pub fn unnormalize_col(expr: Expr) -> Expr {
    expr.transform(|expr| {
        Ok({
            if let Expr::Column(c) = expr {
                let col = Column::new_unqualified(c.name);
                Transformed::yes(Expr::Column(col))
            } else {
                Transformed::no(expr)
            }
        })
    })
    .data()
    .expect("Unnormalize is infallible")
}

/// Create a Column from the Scalar Expr
pub fn create_col_from_scalar_expr(
    scalar_expr: &Expr,
    subqry_alias: String,
) -> Result<Column> {
    match scalar_expr {
        Expr::Alias(Alias { name, .. }) => Ok(Column::new(
            Some::<TableReference>(subqry_alias.into()),
            name,
        )),
        Expr::Column(col) => Ok(col.with_relation(subqry_alias.into())),
        _ => {
            let scalar_column = scalar_expr.schema_name().to_string();
            Ok(Column::new(
                Some::<TableReference>(subqry_alias.into()),
                scalar_column,
            ))
        }
    }
}

/// Recursively un-normalize all [`Column`] expressions in a list of expression trees
#[inline]
pub fn unnormalize_cols(exprs: impl IntoIterator<Item = Expr>) -> Vec<Expr> {
    exprs.into_iter().map(unnormalize_col).collect()
}

/// Recursively remove all the ['OuterReferenceColumn'] and return the inside Column
/// in the expression tree.
pub fn strip_outer_reference(expr: Expr) -> Expr {
    expr.transform(|expr| {
        Ok({
            if let Expr::OuterReferenceColumn(_, col) = expr {
                Transformed::yes(Expr::Column(col))
            } else {
                Transformed::no(expr)
            }
        })
    })
    .data()
    .expect("strip_outer_reference is infallible")
}

/// Returns plan with expressions coerced to types compatible with
/// schema types
pub fn coerce_plan_expr_for_schema(
    plan: LogicalPlan,
    schema: &DFSchema,
) -> Result<LogicalPlan> {
    match plan {
        // special case Projection to avoid adding multiple projections
        LogicalPlan::Projection(Projection { expr, input, .. }) => {
            let new_exprs = coerce_exprs_for_schema(expr, input.schema(), schema)?;
            let projection = Projection::try_new(new_exprs, input)?;
            Ok(LogicalPlan::Projection(projection))
        }
        _ => {
            let exprs: Vec<Expr> = plan.schema().iter().map(Expr::from).collect();
            let new_exprs = coerce_exprs_for_schema(exprs, plan.schema(), schema)?;
            let add_project = new_exprs.iter().any(|expr| expr.try_as_col().is_none());
            if add_project {
                let projection = Projection::try_new(new_exprs, Arc::new(plan))?;
                Ok(LogicalPlan::Projection(projection))
            } else {
                Ok(plan)
            }
        }
    }
}

fn coerce_exprs_for_schema(
    exprs: Vec<Expr>,
    src_schema: &DFSchema,
    dst_schema: &DFSchema,
) -> Result<Vec<Expr>> {
    exprs
        .into_iter()
        .enumerate()
        .map(|(idx, expr)| {
            let new_type = dst_schema.field(idx).data_type();
            if new_type != &expr.get_type(src_schema)? {
                match expr {
                    Expr::Alias(Alias { expr, name, .. }) => {
                        Ok(expr.cast_to(new_type, src_schema)?.alias(name))
                    }
                    #[expect(deprecated)]
                    Expr::Wildcard { .. } => Ok(expr),
                    _ => expr.cast_to(new_type, src_schema),
                }
            } else {
                Ok(expr)
            }
        })
        .collect::<Result<_>>()
}

/// Recursively un-alias an expressions
#[inline]
pub fn unalias(expr: Expr) -> Expr {
    match expr {
        Expr::Alias(Alias { expr, .. }) => unalias(*expr),
        _ => expr,
    }
}

/// Handles ensuring the name of rewritten expressions is not changed.
///
/// This is important when optimizing plans to ensure the output
/// schema of plan nodes don't change after optimization.
/// For example, if an expression `1 + 2` is rewritten to `3`, the name of the
/// expression should be preserved: `3 as "1 + 2"`
///
/// See <https://github.com/apache/datafusion/issues/3555> for details
pub struct NamePreserver {
    use_alias: bool,
}

/// If the qualified name of an expression is remembered, it will be preserved
/// when rewriting the expression
#[derive(Debug)]
pub enum SavedName {
    /// Saved qualified name to be preserved
    Saved {
        relation: Option<TableReference>,
        name: String,
    },
    /// Name is not preserved
    None,
}

impl NamePreserver {
    /// Create a new NamePreserver for rewriting the `expr` that is part of the specified plan
    pub fn new(plan: &LogicalPlan) -> Self {
        Self {
            // The expressions of these plans do not contribute to their output schema,
            // so there is no need to preserve expression names to prevent a schema change.
            use_alias: !matches!(
                plan,
                LogicalPlan::Filter(_)
                    | LogicalPlan::Join(_)
                    | LogicalPlan::TableScan(_)
                    | LogicalPlan::Limit(_)
                    | LogicalPlan::Statement(_)
            ),
        }
    }

    /// Create a new NamePreserver for rewriting the `expr`s in `Projection`
    ///
    /// This will use aliases
    pub fn new_for_projection() -> Self {
        Self { use_alias: true }
    }

    pub fn save(&self, expr: &Expr) -> SavedName {
        if self.use_alias {
            let (relation, name) = expr.qualified_name();
            SavedName::Saved { relation, name }
        } else {
            SavedName::None
        }
    }
}

impl SavedName {
    /// Ensures the qualified name of the rewritten expression is preserved
    pub fn restore(self, expr: Expr) -> Expr {
        match self {
            SavedName::Saved { relation, name } => {
                let (new_relation, new_name) = expr.qualified_name();
                if new_relation != relation || new_name != name {
                    expr.alias_qualified(relation, name)
                } else {
                    expr
                }
            }
            SavedName::None => expr,
        }
    }
}

#[cfg(test)]
mod test {
    use std::ops::Add;

    use super::*;
    use crate::literal::lit_with_metadata;
    use crate::{col, lit, Cast};
    use arrow::datatypes::{DataType, Field, Schema};
    use datafusion_common::tree_node::TreeNodeRewriter;
    use datafusion_common::ScalarValue;

    #[derive(Default)]
    struct RecordingRewriter {
        v: Vec<String>,
    }

    impl TreeNodeRewriter for RecordingRewriter {
        type Node = Expr;

        fn f_down(&mut self, expr: Expr) -> Result<Transformed<Expr>> {
            self.v.push(format!("Previsited {expr}"));
            Ok(Transformed::no(expr))
        }

        fn f_up(&mut self, expr: Expr) -> Result<Transformed<Expr>> {
            self.v.push(format!("Mutated {expr}"));
            Ok(Transformed::no(expr))
        }
    }

    #[test]
    fn rewriter_rewrite() {
        // rewrites all "foo" string literals to "bar"
        let transformer = |expr: Expr| -> Result<Transformed<Expr>> {
            match expr {
                Expr::Literal(ScalarValue::Utf8(Some(utf8_val)), metadata) => {
                    let utf8_val = if utf8_val == "foo" {
                        "bar".to_string()
                    } else {
                        utf8_val
                    };
                    Ok(Transformed::yes(lit_with_metadata(utf8_val, metadata)))
                }
                // otherwise, return None
                _ => Ok(Transformed::no(expr)),
            }
        };

        // rewrites "foo" --> "bar"
        let rewritten = col("state")
            .eq(lit("foo"))
            .transform(transformer)
            .data()
            .unwrap();
        assert_eq!(rewritten, col("state").eq(lit("bar")));

        // doesn't rewrite
        let rewritten = col("state")
            .eq(lit("baz"))
            .transform(transformer)
            .data()
            .unwrap();
        assert_eq!(rewritten, col("state").eq(lit("baz")));
    }

    #[test]
    fn normalize_cols() {
        let expr = col("a") + col("b") + col("c");

        // Schemas with some matching and some non matching cols
        let schema_a = make_schema_with_empty_metadata(
            vec![Some("tableA".into()), Some("tableA".into())],
            vec!["a", "aa"],
        );
        let schema_c = make_schema_with_empty_metadata(
            vec![Some("tableC".into()), Some("tableC".into())],
            vec!["cc", "c"],
        );
        let schema_b =
            make_schema_with_empty_metadata(vec![Some("tableB".into())], vec!["b"]);
        // non matching
        let schema_f = make_schema_with_empty_metadata(
            vec![Some("tableC".into()), Some("tableC".into())],
            vec!["f", "ff"],
        );
        let schemas = vec![schema_c, schema_f, schema_b, schema_a];
        let schemas = schemas.iter().collect::<Vec<_>>();

        let normalized_expr =
            normalize_col_with_schemas_and_ambiguity_check(expr, &[&schemas], &[])
                .unwrap();
        assert_eq!(
            normalized_expr,
            col("tableA.a") + col("tableB.b") + col("tableC.c")
        );
    }

    #[test]
    fn normalize_cols_non_exist() {
        // test normalizing columns when the name doesn't exist
        let expr = col("a") + col("b");
        let schema_a =
            make_schema_with_empty_metadata(vec![Some("\"tableA\"".into())], vec!["a"]);
        let schemas = [schema_a];
        let schemas = schemas.iter().collect::<Vec<_>>();

        let error =
            normalize_col_with_schemas_and_ambiguity_check(expr, &[&schemas], &[])
                .unwrap_err()
                .strip_backtrace();
        let expected = "Schema error: No field named b. \
            Valid fields are \"tableA\".a.";
        assert_eq!(error, expected);
    }

    #[test]
    fn unnormalize_cols() {
        let expr = col("tableA.a") + col("tableB.b");
        let unnormalized_expr = unnormalize_col(expr);
        assert_eq!(unnormalized_expr, col("a") + col("b"));
    }

    fn make_schema_with_empty_metadata(
        qualifiers: Vec<Option<TableReference>>,
        fields: Vec<&str>,
    ) -> DFSchema {
        let fields = fields
            .iter()
            .map(|f| Arc::new(Field::new(f.to_string(), DataType::Int8, false)))
            .collect::<Vec<_>>();
        let schema = Arc::new(Schema::new(fields));
        DFSchema::from_field_specific_qualified_schema(qualifiers, &schema).unwrap()
    }

    #[test]
    fn rewriter_visit() {
        let mut rewriter = RecordingRewriter::default();
        col("state").eq(lit("CO")).rewrite(&mut rewriter).unwrap();

        assert_eq!(
            rewriter.v,
            vec![
                "Previsited state = Utf8(\"CO\")",
                "Previsited state",
                "Mutated state",
                "Previsited Utf8(\"CO\")",
                "Mutated Utf8(\"CO\")",
                "Mutated state = Utf8(\"CO\")"
            ]
        )
    }

    #[test]
    fn test_rewrite_preserving_name() {
        test_rewrite(col("a"), col("a"));

        test_rewrite(col("a"), col("b"));

        // cast data types
        test_rewrite(
            col("a"),
            Expr::Cast(Cast::new(Box::new(col("a")), DataType::Int32)),
        );

        // change literal type from i32 to i64
        test_rewrite(col("a").add(lit(1i32)), col("a").add(lit(1i64)));

        // test preserve qualifier
        test_rewrite(
            Expr::Column(Column::new(Some("test"), "a")),
            Expr::Column(Column::new_unqualified("test.a")),
        );
        test_rewrite(
            Expr::Column(Column::new_unqualified("test.a")),
            Expr::Column(Column::new(Some("test"), "a")),
        );
    }

    /// rewrites `expr_from` to `rewrite_to` while preserving the original qualified name
    /// by using the `NamePreserver`
    fn test_rewrite(expr_from: Expr, rewrite_to: Expr) {
        struct TestRewriter {
            rewrite_to: Expr,
        }

        impl TreeNodeRewriter for TestRewriter {
            type Node = Expr;

            fn f_up(&mut self, _: Expr) -> Result<Transformed<Expr>> {
                Ok(Transformed::yes(self.rewrite_to.clone()))
            }
        }

        let mut rewriter = TestRewriter {
            rewrite_to: rewrite_to.clone(),
        };
        let saved_name = NamePreserver { use_alias: true }.save(&expr_from);
        let new_expr = expr_from.clone().rewrite(&mut rewriter).unwrap().data;
        let new_expr = saved_name.restore(new_expr);

        let original_name = expr_from.qualified_name();
        let new_name = new_expr.qualified_name();
        assert_eq!(
            original_name, new_name,
            "mismatch rewriting expr_from: {expr_from} to {rewrite_to}"
        )
    }
}
