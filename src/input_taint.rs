//! TODO: comment on what we allow and what we don't, emphasize that we're starting things off
//! pretty conservative.
use crate::builtins::Variable;
use crate::bytecode::{Accum, Instr};
use crate::common::{FileSpec, Graph, NodeIx, NumTy, WorkList};
use crate::compile::{HighLevel, Ty};

use hashbrown::HashMap;
use petgraph::Direction;

#[derive(Eq, PartialEq, Hash, Clone)]
enum Key {
    Reg(NumTy, Ty),
    Rng,
    Var(Variable, Ty),
    Slot(i64, Ty),
    Func(NumTy),
}

impl<'a, T: Accum> From<&'a T> for Key {
    fn from(t: &T) -> Key {
        let (reg, ty) = t.reflect();
        Key::Reg(reg, ty)
    }
}

#[derive(Default)]
pub struct TaintedStringAnalysis {
    flows: Graph</*tainted=*/ bool, ()>,
    regs: HashMap<Key, NodeIx>,
    queries: Vec<Key>,
    wl: WorkList<NodeIx>,
}

impl TaintedStringAnalysis {
    pub(crate) fn visit_hl(&mut self, cur_fn_id: NumTy, inst: &HighLevel) {
        // A little on how this handles functions. We do not implement a call-sensitive analysis.
        // Instead, we ask "is the return value of a function always tainted by user input?" when
        // analyzing the function body and "are any of the arguments tainted by user input" at the
        // call site. This is a bit simplistic, i.e. it rules out scripts like:
        //
        // function cmd(x, y) { return ($0) ? x : y; }
        // { print "X" | cmd("tee non-empty-line", "tee empty-line") }
        //
        // Which should be safe.
        use HighLevel::*;
        match inst {
            Call {
                func_id,
                dst_reg,
                dst_ty,
                args,
            } => {
                let dst_key = Key::Reg(*dst_reg, *dst_ty);
                self.add_dep(dst_key.clone(), Key::Func(*func_id));
                for (reg, ty) in args.iter().cloned() {
                    self.add_dep(dst_key.clone(), Key::Reg(reg, ty));
                }
            }
            Ret(reg, ty) => {
                self.add_dep(Key::Func(cur_fn_id), Key::Reg(*reg, *ty));
            }
            Phi(reg, ty, preds) => {
                for (_, pred_reg) in preds.iter() {
                    self.add_dep(Key::Reg(*reg, *ty), Key::Reg(*pred_reg, *ty));
                }
            }
            DropIter(..) => {}
        }
    }
    pub(crate) fn visit_ll<'a>(&mut self, inst: &Instr<'a>) {
        // NB: this analysis currently tracks taint even in string-to-integer operations. I cannot
        // currently think of any security issues around interpolating an arbitrary integer (or
        // float, though perhaps that is more plausible) into a shell command. It's a easy and
        // mechanical fix to break the chain of infection on integer boundaries like this, but we
        // should read up on the potential attack surface first.
        use Instr::*;
        match inst {
            StoreConstStr(dst, _) => self.add_src(dst, /*tainted=*/ false),
            StoreConstInt(dst, _) => self.add_src(dst, /*tainted=*/ false),
            StoreConstFloat(dst, _) => self.add_src(dst, /*tainted=*/ false),

            IntToStr(dst, src) => self.add_dep(dst, src),
            IntToFloat(dst, src) => self.add_dep(dst, src),
            FloatToStr(dst, src) => self.add_dep(dst, src),
            FloatToInt(dst, src) => self.add_dep(dst, src),
            StrToFloat(dst, src) => self.add_dep(dst, src),
            LenStr(dst, src) | StrToInt(dst, src) | HexStrToInt(dst, src) => self.add_dep(dst, src),

            Mov(ty, dst, src) => self.add_dep(Key::Reg(*dst, *ty), Key::Reg(*src, *ty)),
            AddInt(dst, x, y)
            | MulInt(dst, x, y)
            | MinusInt(dst, x, y)
            | ModInt(dst, x, y)
            | Int2(_, dst, x, y) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
            }
            AddFloat(dst, x, y)
            | MulFloat(dst, x, y)
            | MinusFloat(dst, x, y)
            | ModFloat(dst, x, y)
            | Div(dst, x, y)
            | Pow(dst, x, y)
            | Float2(_, dst, x, y) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
            }

            Not(dst, src) | NegInt(dst, src) | Int1(_, dst, src) => self.add_dep(dst, src),
            NegFloat(dst, src) | Float1(_, dst, src) => self.add_dep(dst, src),
            NotStr(dst, src) => self.add_dep(dst, src),
            Rand(dst) => self.add_dep(dst, Key::Rng),
            Srand(old, new) => {
                self.add_dep(old, Key::Rng);
                self.add_dep(Key::Rng, new);
            }
            ReseedRng(new) => self.add_dep(Key::Rng, new),
            Concat(dst, x, y) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
            }
            IsMatch(dst, x, y) | Match(dst, x, y) | SubstrIndex(dst, x, y) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
            }
            GSub(dst, x, y, dstin) | Sub(dst, x, y, dstin) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
                self.add_dep(dstin, x);
                self.add_dep(dstin, y);
            }
            EscapeTSV(dst, src) | EscapeCSV(dst, src) => self.add_dep(dst, src),
            Substr(dst, x, y, z) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
                self.add_dep(dst, z);
            }
            LTFloat(dst, x, y)
            | GTFloat(dst, x, y)
            | LTEFloat(dst, x, y)
            | GTEFloat(dst, x, y)
            | EQFloat(dst, x, y) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
            }
            LTInt(dst, x, y)
            | GTInt(dst, x, y)
            | LTEInt(dst, x, y)
            | GTEInt(dst, x, y)
            | EQInt(dst, x, y) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
            }
            LTStr(dst, x, y)
            | GTStr(dst, x, y)
            | LTEStr(dst, x, y)
            | GTEStr(dst, x, y)
            | EQStr(dst, x, y) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
            }
            GetColumn(dst, _) => self.add_src(dst, true),
            JoinTSV(dst, start, end) | JoinCSV(dst, start, end) => {
                self.add_dep(dst, start);
                self.add_dep(dst, end);
            }
            JoinColumns(dst, x, y, z) => {
                self.add_dep(dst, x);
                self.add_dep(dst, y);
                self.add_dep(dst, z);
            }
            // maybe a bit paranoid, but may as well.
            ReadErr(dst, cmd, is_file) => {
                self.add_src(dst, true);
                if !*is_file {
                    self.queries.push(cmd.into());
                }
            },
            NextLine(dst, cmd, is_file) => {
                self.add_src(dst, true);
                if !*is_file {
                    self.queries.push(cmd.into())
                }
            },
            ReadErrStdin(dst) => self.add_src(dst, true),
            NextLineStdin(dst) => self.add_src(dst, true),
            SplitInt(dst1, src1, dst2, src2) => {
                self.add_dep(dst1, src1);
                self.add_dep(dst1, src2);
                self.add_dep(dst2, src1);
                self.add_dep(dst2, src2);
            }
            SplitStr(dst1, src1, dst2, src2) => {
                self.add_dep(dst1, src1);
                self.add_dep(dst1, src2);
                self.add_dep(dst2, src1);
                self.add_dep(dst2, src2);
            }
            Sprintf { dst, fmt, args } => {
                self.add_dep(dst, fmt);
                for (reg, ty) in args.iter() {
                    self.add_dep(dst, Key::Reg(*reg, *ty));
                }
            }
            Printf {
                output: Some((cmd, FileSpec::Cmd)),
                ..
            } => self.queries.push(cmd.into()),
            Print(_, out, FileSpec::Cmd) => self.queries.push(out.into()),
            Lookup {
                map_ty,
                dst,
                map,
                ..
            } => self.add_dep(
                Key::Reg(*dst, map_ty.val().unwrap()),
                Key::Reg(*map, *map_ty),
            ),
            Len { map_ty, dst, map } => self.add_dep(Key::Reg(*dst, Ty::Int), Key::Reg(*map, *map_ty)),
            Store { map_ty, map, key, val } => {
                self.add_dep(Key::Reg(*map, *map_ty), Key::Reg(*key, map_ty.key().unwrap()));
                self.add_dep(Key::Reg(*map, *map_ty), Key::Reg(*val, map_ty.val().unwrap()));
            }
            IterBegin { map_ty, dst, map } => {
                self.add_dep(Key::Reg(*dst, map_ty.key_iter().unwrap()), Key::Reg(*map, *map_ty));
            }
            IterGetNext{iter_ty, dst, iter} => {
                self.add_dep(Key::Reg(*dst, iter_ty.iter().unwrap()), Key::Reg(*iter, *iter_ty));
            }
            LoadVarStr(dst, v) => self.add_dep(dst, Key::Var(*v, Ty::Str)),
            LoadVarInt(dst, v) => self.add_dep(dst, Key::Var(*v, Ty::Int)),
            LoadVarIntMap(dst, v) => self.add_dep(dst, Key::Var(*v, Ty::MapIntStr)),
            StoreVarStr(v, src) => self.add_dep(Key::Var(*v, Ty::Str), src),
            StoreVarInt(v, src) => self.add_dep(Key::Var(*v, Ty::Int), src),
            StoreVarIntMap(v, src) => self.add_dep(Key::Var(*v, Ty::MapIntStr), src),
            LoadSlot{ty,slot,dst} => self.add_dep(Key::Reg(*dst, *ty), Key::Slot(*slot, *ty)),
            StoreSlot{ty,slot,src} => self.add_dep(Key::Slot(*slot, *ty), Key::Reg(*src, *ty)),
            Delete{..}
            | Contains{..} // 0 or 1
            | IterHasNext{..}
            |JmpIf(..)
            |Jmp(_)
            | Halt
            | Push(..)
            | Pop(..)
            // We consume high-level instructions, so calls and returns are handled by visit_hl
            // above
            | Call(_)
            | Ret
            | Printf { .. }
            | PrintStdout(_)
            | Print(..)
            | Close(_)
            | NextLineStdinFused()
            | NextFile()
            | SetColumn(_, _)
            | AllocMap(_, _) => {}
        }
    }
    fn get_node(&mut self, k: Key) -> NodeIx {
        let flows = &mut self.flows;
        let wl = &mut self.wl;
        self.regs
            .entry(k)
            .or_insert_with(|| {
                let ix = flows.add_node(false);
                wl.insert(ix);
                ix
            })
            .clone()
    }
    fn add_dep(&mut self, dst_reg: impl Into<Key>, src_reg: impl Into<Key>) {
        let src_node = self.get_node(src_reg.into());
        let dst_node = self.get_node(dst_reg.into());
        self.flows.add_edge(src_node, dst_node, ());
    }
    fn add_src(&mut self, reg: impl Into<Key>, tainted: bool) {
        let ix = self.get_node(reg.into());
        let w = self.flows.node_weight_mut(ix).unwrap();
        if *w != tainted {
            *w = tainted;
            self.wl.insert(ix);
        }
    }

    pub(crate) fn ok(&mut self) -> bool {
        // TODO: add context to the "false" case here.
        if self.queries.len() == 0 {
            return true;
        }
        self.solve();
        for q in self.queries.iter() {
            if *self.flows.node_weight(self.regs[q]).unwrap() {
                return false;
            }
        }
        true
    }

    fn solve(&mut self) {
        while let Some(n) = self.wl.pop() {
            let start = *self.flows.node_weight(n).unwrap();
            if start {
                continue;
            }
            let mut new = start;
            for n in self.flows.neighbors_directed(n, Direction::Incoming) {
                new |= *self.flows.node_weight(n).unwrap();
            }
            if !new {
                continue;
            }
            *self.flows.node_weight_mut(n).unwrap() = new;
            for n in self.flows.neighbors_directed(n, Direction::Outgoing) {
                self.wl.insert(n)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::common::Result;

    fn compiles(p: &str, allow_arbitrary: bool) -> Result<()> {
        use crate::arena::Arena;
        use crate::ast;
        use crate::cfg;
        use crate::common::{ExecutionStrategy, Stage};
        use crate::compile;
        use crate::lexer::Tokenizer;
        use crate::parsing::syntax::ProgParser;
        use crate::runtime::{
            self,
            splitter::batch::{CSVReader, InputFormat},
        };

        use std::io;
        let a = Arena::default();
        let prog = a.alloc_str(p);
        let lexer = Tokenizer::new(prog);
        let mut buf = Vec::new();
        let parser = ProgParser::new();
        let mut prog = ast::Prog::from_stage(Stage::Main(()));
        if let Err(e) = parser.parse(&a, &mut buf, &mut prog, lexer) {
            return err!("parse failure: {}", e);
        }
        let mut ctx = cfg::ProgramContext::from_prog(&a, a.alloc_v(prog), cfg::Escaper::default())?;
        ctx.allow_arbitrary_commands = allow_arbitrary;
        let fake_inp: Box<dyn io::Read + Send> = Box::new(std::io::Cursor::new(vec![]));
        compile::bytecode(
            &mut ctx,
            CSVReader::new(
                std::iter::once((fake_inp, String::from("unused"))),
                InputFormat::CSV,
                /*chunk_size=*/ 1024,
                /*check_utf8=*/ false,
                ExecutionStrategy::Serial,
            ),
            runtime::writers::default_factory(),
            /*num_workers=*/ 1,
        )?;
        Ok(())
    }

    fn assert_analysis_reject(p: &str) {
        compiles(p, /*allow_arbitrary=*/ true)
            .expect(format!("program should compile without taint checks prog=[{}]", p).as_str());
        assert!(compiles(p, /*allow_arbitrary=*/ false).is_err());
    }

    fn assert_analysis_accept(p: &str) {
        compiles(p, /*allow_arbitrary=*/ true)
            .expect(format!("program should compile without taint checks prog=[{}]", p).as_str());
        compiles(p, /*allow_arbitrary=*/ false).expect("taint analysis should pass");
    }

    #[test]
    fn rules_out() {
        let progs: &[&str] = &[
            "BEGIN { print $1 | $2; }",
            "BEGIN { while ($1 | getline) print; }",
            "BEGIN { while (length($1) | getline) print; }",
            r#"BEGIN { while (getline x) print y | ("echo " x); }"#,
            "BEGIN { if ($2) { j = $1 3; x=j;} print y | x }",
            r#"function x(a, b) { return $2 a b; }
            BEGIN { while (x(2, 3) | getline) print; }"#,
            r#"function x(a, b) { return a b; }
            BEGIN {  print "hello" | x($2, "dog"); }"#,
        ];

        for p in progs.iter() {
            assert_analysis_reject(*p);
        }
    }

    #[test]
    fn rules_in() {
        let progs: &[&str] = &[
            r#"BEGIN { print "hello" | "command"; }"#,
            r#"BEGIN { while ("command" | getline) print; }"#,
            r#"BEGIN { if ($1) x=5; else y="hi"; print "should work" | x; }"#,
            r#"function x(a, b) { print $2; return a b;}
            BEGIN { while(x("echo ", "hi") | getline) print; }"#,
        ];
        for p in progs.iter() {
            assert_analysis_accept(*p);
        }
    }
}
