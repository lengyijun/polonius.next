#[cfg(test)]
mod test;

use crate::ast::*;
use crate::ast_parser::parse_ast;
use std::collections::{BTreeMap, HashMap};
use std::fmt;

#[derive(Default, PartialEq, Eq, Clone)]
struct Origin(String);

#[derive(Default, PartialEq, Eq, Clone)]
struct Node(String);

impl<S> From<S> for Origin
where
    S: AsRef<str> + ToString,
{
    fn from(s: S) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl<S> From<S> for Node
where
    S: AsRef<str> + ToString,
{
    fn from(s: S) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

#[derive(Default, Debug)]
struct Facts {
    access_origin: Vec<(Origin, Node)>,
    cfg_edge: Vec<(Node, Node)>,
    clear_origin: Vec<(Origin, Node)>,
    introduce_subset: Vec<(Origin, Origin, Node)>,
    invalidate_origin: Vec<(Origin, Node)>,
    node_text: Vec<(String, Node)>,
}

#[allow(dead_code)]
fn emit_facts(input: &str) -> eyre::Result<Facts> {
    let program = parse_ast(input)?;
    let emitter = FactEmitter::new(program, input);
    let mut facts = Default::default();
    emitter.emit_facts(&mut facts);
    Ok(facts)
}

// An internal representation of a `Node`, a location in the CFG: the block within the program,
// and the statement within that block. Used to analyze locations (e.g. reachability), whereas
// `Node`s are user-readable representations for facts.
#[allow(dead_code)]
struct Location {
    block_idx: usize,
    statement_idx: usize,
}

impl From<(usize, usize)> for Location {
    fn from((block_idx, statement_idx): (usize, usize)) -> Self {
        Self {
            block_idx,
            statement_idx,
        }
    }
}

struct FactEmitter<'a> {
    input: &'a str,
    program: Program,
    loans: HashMap<Place, Vec<(Origin, Location)>>,
}

impl<'a> FactEmitter<'a> {
    fn new(program: Program, input: &'a str) -> Self {
        // Collect loans from borrow expressions present in the program
        let mut loans: HashMap<Place, Vec<(Origin, Location)>> = HashMap::new();

        for (block_idx, bb) in program.basic_blocks.iter().enumerate() {
            for (statement_idx, s) in bb.statements.iter().enumerate() {
                let (Statement::Assign(_, expr) | Statement::Expr(expr)) = &**s;

                if let Expr::Access {
                    kind: AccessKind::Borrow(origin) | AccessKind::BorrowMut(origin),
                    place,
                } = expr
                {
                    // TODO: handle fields and loans taken on subsets of their paths.
                    // Until then: only support borrowing from complete places.
                    //
                    // TODO: we probably also need to track the loan's mode, if we want to emit
                    // errors when mutably borrowing through a shared ref and the likes ?
                    loans
                        .entry(place.clone())
                        .or_default()
                        .push((origin.into(), (block_idx, statement_idx).into()));
                }
            }
        }

        Self {
            input,
            program,
            loans,
        }
    }

    fn emit_facts(&self, facts: &mut Facts) {
        for bb in &self.program.basic_blocks {
            self.emit_block_facts(bb, facts);
        }
    }

    fn emit_block_facts(&self, bb: &BasicBlock, facts: &mut Facts) {
        // Emit CFG facts for the block
        self.emit_cfg_edges(&bb, facts);

        for (idx, s) in bb.statements.iter().enumerate() {
            let node = self.node_at(&bb.name, idx);

            // Emit `node_text` for this statement: the line from where it was parsed
            // in the original input program.
            let statement_text = {
                let span = s.span();
                self.input[span.start()..span.end() - 1].to_string()
            };
            facts.node_text.push((statement_text, node.clone()));

            match &**s {
                Statement::Assign(place, expr) => {
                    // Emit facts about the assignment LHS
                    let (lhs_ty, lhs_origins) = self.ty_and_origins_of_place(place);

                    // Assignments clear all origins in the type
                    for origin in &lhs_origins {
                        facts.clear_origin.push((origin.clone(), node.clone()));
                    }

                    if !lhs_ty.is_ref() {
                        // Assignments to non-references invalidate loans borrowing from them.
                        //
                        // TODO: handle assignments to fields and loans taken on subsets of
                        // their paths. Until then: only support invalidations on assignments
                        // to complete places.
                        //
                        if let Some(loans) = self.loans.get(place) {
                            for (origin, _location) in loans {
                                // TODO: if the `location` where the loan was issued can't
                                // reach the current location, there is no need to emit
                                // the invalidation
                                facts.invalidate_origin.push((origin.clone(), node.clone()));
                            }
                        }
                    }

                    // Emit facts about the assignment RHS: evaluate the `expr`
                    self.emit_expr_facts(&node, expr, facts);

                    // Relate the LHS and RHS tys
                    self.emit_subset_facts(&node, &lhs_ty, expr, facts);
                }

                Statement::Expr(expr) => {
                    // Evaluate the `expr`
                    self.emit_expr_facts(&node, expr, facts);
                }
            }
        }
    }

    fn emit_expr_facts(&self, node: &Node, expr: &Expr, facts: &mut Facts) {
        match expr {
            Expr::Access { kind, place } => {
                match kind {
                    // Borrowing clears its origin: it's issuing a fresh origin of the same name
                    AccessKind::Borrow(origin) | AccessKind::BorrowMut(origin) => {
                        facts.clear_origin.push((origin.into(), node.clone()));

                        if matches!(kind, AccessKind::BorrowMut(_)) {
                            // A mutable borrow is considered a write to the place:
                            //
                            // 1) it accesses the origins in the type
                            let (_, origins) = self.ty_and_origins_of_place(place);
                            for origin in origins {
                                facts.access_origin.push((origin.clone(), node.clone()));
                            }

                            // 2) and invalidates existing loans of that place
                            //
                            // TODO: handle assignments to fields and loans taken on subsets of
                            // their paths. Until then: only support invalidations on assignments
                            // to complete places.
                            //
                            // TODO: here as well, there is a question of: can the loans we're
                            // invalidating, reach the current node ?
                            //
                            if let Some(loans) = self.loans.get(place) {
                                for (origin, _) in loans {
                                    facts.invalidate_origin.push((origin.clone(), node.clone()));
                                }
                            }
                        }
                    }

                    AccessKind::Copy | AccessKind::Move => {
                        // FIXME: currently function call parameters are not parsed without access
                        // kinds, check if there's some special behaviour needed for copy/moves,
                        // instead of just being "reads" (e.g. maybe moves also need clearing
                        // or invalidations)

                        // Reads access all the origins in their type
                        let (_, origins) = self.ty_and_origins_of_place(place);
                        for origin in origins {
                            facts.access_origin.push((origin.into(), node.clone()));
                        }
                    }
                }
            }

            Expr::Call { arguments, .. } => {
                // Calls evaluate their arguments
                arguments
                    .iter()
                    .for_each(|expr| self.emit_expr_facts(&node, expr, facts));

                // TODO: Depending on the signature of the function, some subsets can be introduced
                // between the arguments to the call
            }

            _ => {}
        }
    }

    // Introduce subsets: `expr` flows into `place`
    //
    // TODO: do we need some type checking to ensure this assigment is valid
    // with respect to the LHS/RHS types, mutability, etc ?
    //
    // TODO: handles simple subsets only for now, complete this.
    //
    // TODO: if the `expr` is a call, we probably also need subsets between
    // the arguments, the return value and the LHS ?
    //
    // We're in an assignment and we assume the LHS and RHS have the same shape,
    // for example `&'a Type<&'b i32> = &'1 Type<'2 i32>`.
    //
    fn emit_subset_facts(&self, node: &Node, lhs_ty: &Ty, rhs_expr: &Expr, facts: &mut Facts) {
        match lhs_ty {
            Ty::Ref {
                origin: target_origin,
                ..
            }
            | Ty::RefMut {
                origin: target_origin,
                ..
            } => {
                let mut emit_subset_fact = |source_origin, target_origin| {
                    facts
                        .introduce_subset
                        .push((source_origin, target_origin, node.clone()));
                };

                match rhs_expr {
                    Expr::Access {
                        kind:
                            AccessKind::Borrow(source_origin) | AccessKind::BorrowMut(source_origin),
                        ..
                    } => {
                        emit_subset_fact(source_origin.into(), target_origin.into());
                    }

                    Expr::Access {
                        kind: AccessKind::Copy | AccessKind::Move,
                        place,
                    } => {
                        let (rhs_ty, _) = self.ty_and_origins_of_place(place);
                        match rhs_ty {
                            Ty::Ref {
                                origin: source_origin,
                                ..
                            }
                            | Ty::RefMut {
                                origin: source_origin,
                                ..
                            } => {
                                emit_subset_fact(source_origin.into(), target_origin.into());
                            }

                            _ => {
                                // The RHS has no refs, there are no subsets to emit
                            }
                        }
                    }

                    _ => {
                        // The expr is not borrowing anything, there are no
                        // subsets to emit
                    }
                }
            }

            _ => {
                // The LHS contains no origins, there are no subsets to emit
            }
        }
    }

    fn emit_cfg_edges(&self, bb: &BasicBlock, facts: &mut Facts) {
        let statement_count = bb.statements.len();

        // Emit intra-block CFG edges between statements
        for idx in 1..statement_count {
            facts
                .cfg_edge
                .push((self.node_at(&bb.name, idx - 1), self.node_at(&bb.name, idx)));
        }

        // Emit inter-block CFG edges between a block and its successors
        for succ in &bb.successors {
            // Note: `goto`s are not statements, so a block with a single goto
            // has no statements but still needs a node index in the CFG.
            facts.cfg_edge.push((
                self.node_at(&bb.name, statement_count.saturating_sub(1)),
                self.node_at(succ, 0),
            ));
        }
    }

    fn ty_and_origins_of_place(&self, place: &Place) -> (&Ty, Vec<Origin>) {
        let mut origins = Vec::new();

        // The `base` is always a variable of the program, but can be deref'd.
        let base = if let Some(deref_base) = place.deref_base() {
            deref_base
        } else {
            &place.base
        };

        let v = self
            .program
            .variables
            .iter()
            .find(|v| v.name == base)
            .unwrap_or_else(|| panic!("Can't find variable {}", place.base));

        let ty = if place.fields.is_empty() {
            &v.ty
        } else {
            // If there are any fields, then this must be a struct
            assert!(matches!(v.ty, Ty::Struct { .. }));

            // Find the type of each field in sequence, to return the last field's type
            place.fields.iter().fold(&v.ty, |ty, field_name| {
                // Collect origins present in the current field parent's ty
                ty.collect_origins_into(&mut origins);

                // Find the struct decl for the parent's ty
                let (struct_name, struct_substs) = match ty {
                    Ty::Struct { name, parameters } => (name, parameters),
                    _ => panic!("Ty {:?} must be a struct to access its fields", ty),
                };
                let decl = self
                    .program
                    .struct_decls
                    .iter()
                    .find(|s| &s.name == struct_name)
                    .unwrap_or_else(|| {
                        panic!("Can't find struct {} at field {}", struct_name, field_name,)
                    });

                // Find the expected named field inside the struct decl
                let field = decl
                    .field_decls
                    .iter()
                    .find(|v| &v.name == field_name)
                    .unwrap_or_else(|| {
                        panic!("Can't find field {} in struct {}", field_name, struct_name)
                    });

                // It's possible that the field has a generic type, which we need to substitute
                // with the matching type from the struct's arguments
                match &field.ty {
                    Ty::Struct {
                        name: field_ty_name,
                        ..
                    } => {
                        if let Some(idx) = decl.generic_decls.iter().position(|d| match d {
                            GenericDecl::Ty(param_ty_name) => param_ty_name == field_ty_name,
                            _ => false,
                        }) {
                            // We found the field ty in the generic decls, so return the subst
                            // at the same index
                            match &struct_substs[idx] {
                                Parameter::Ty(subst_ty) => subst_ty,

                                // TODO: handle generic origins
                                _ => panic!("The parameter at idx {} should be a Ty", idx),
                            }
                        } else {
                            // Otherwise, the field ty is a regular type
                            &field.ty
                        }
                    }
                    _ => &field.ty,
                }
            })
        };

        // Collect origins for either:
        // - the `base` ty, when there are no fields
        // - the last field's ty, from the place's `fields` list. The origins of the previous
        // fields in the list having already been collected just above.
        ty.collect_origins_into(&mut origins);

        (ty, origins)
    }

    fn node_at(&self, block: &str, statement_idx: usize) -> Node {
        let mut node = format!("{}[{}]", block, statement_idx);

        // Hack: if we temporarily need simpler node names, while comparing to the manual facts:
        // use single-letter names.
        if std::env::var("SIMPLE_NODES").is_ok() {
            // Make the block-local statement idx refer to a concatenated list of all
            // statements: adding the number of statements prior to this block.
            let bb_statement_start_idx = self
                .program
                .basic_blocks
                .iter()
                .take_while(|bb| block != bb.name)
                .fold(0, |acc, bb| acc + bb.statements.len());
            let node_idx = 'a' as u32 + (bb_statement_start_idx + statement_idx) as u32;
            let node_as_letter = char::from_u32(node_idx).unwrap_or_else(|| {
                panic!(
                    "Couldn't turn '{}' into a single letter name for node {:?}",
                    node_idx, node
                )
            });
            node = node_as_letter.to_string().into()
        }

        node.into()
    }
}

impl Ty {
    fn is_ref(&self) -> bool {
        matches!(self, Ty::Ref { .. } | Ty::RefMut { .. })
    }

    // Collects all the origins present in this type, recursively.
    fn collect_origins_into(&self, origins: &mut Vec<Origin>) {
        match self {
            Ty::Ref { origin, ty } | Ty::RefMut { origin, ty } => {
                origins.push(origin.into());
                ty.collect_origins_into(origins);
            }

            Ty::Struct { parameters, .. } => {
                for param in parameters {
                    match param {
                        Parameter::Origin(origin) => origins.push(origin.into()),
                        Parameter::Ty(ty) => ty.collect_origins_into(origins),
                    }
                }
            }

            Ty::I32 => {}
            Ty::Unit => {}
        }
    }
}

impl Place {
    // If the place `base` is deref'd, returns its name without the deref `*`
    fn deref_base(&self) -> Option<&str> {
        if self.base.starts_with('*') {
            Some(&self.base[1..])
        } else {
            None
        }
    }
}

// For readability purposes, and conversion to Soufflé facts, display the facts as the
// textual format.
impl fmt::Display for Facts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Index facts to group them per node
        let mut facts_per_node: BTreeMap<&str, Vec<String>> = BTreeMap::new();

        // Until fact gen is complete, some nodes present in the input program may not
        // have corresponding facts here, so ensure nodes present in CFG edges are
        // created empty.
        //
        // Single statement programs with no facts will still not create empty points though,
        // for that we could use the `ast::Program` as input for this impl.
        //
        // (And we then could add the decls as comments, like the examples currently have)
        //
        for (node1, node2) in &self.cfg_edge {
            facts_per_node.entry(&node1.0).or_default();
            facts_per_node.entry(&node2.0).or_default();
        }

        // Display the facts in the operational order described in the datalog rules.
        for (origin, node) in &self.access_origin {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("access_origin({})", origin.0));
        }

        for (origin, node) in &self.invalidate_origin {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("invalidate_origin({})", origin.0));
        }

        for (origin, node) in &self.clear_origin {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("clear_origin({})", origin.0));
        }

        for (origin1, origin2, node) in &self.introduce_subset {
            facts_per_node
                .entry(&node.0)
                .or_default()
                .push(format!("introduce_subset({}, {})", origin1.0, origin2.0));
        }

        // Display the indexed data in the frontend format
        for (node_idx, (node, facts)) in facts_per_node.into_iter().enumerate() {
            if node_idx != 0 {
                write!(f, "\n")?;
            }

            // Emit node start, with the statement's `node_text` representation
            let node_text = self
                .node_text
                .iter()
                .find_map(|(node_text, candidate_node)| {
                    if candidate_node.0 == node {
                        Some(node_text.as_ref())
                    } else {
                        None
                    }
                })
                .unwrap_or("(pass)");
            writeln!(f, "{}: {:?} {{", node, node_text)?;

            // Emit all facts first
            for fact in facts {
                writeln!(f, "\t{}", fact)?;
            }

            // And `goto` facts last, with their special syntax. A `goto` is always required,
            // even for the function's exit node (but will have no successors in that case).
            write!(f, "\tgoto")?;
            for (_, succ) in self.cfg_edge.iter().filter(|(from, _)| from.0 == node) {
                write!(f, " {}", succ.0)?;
            }

            writeln!(f, "\n}}")?;
        }

        Ok(())
    }
}
