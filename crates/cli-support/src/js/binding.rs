//! Support for actually generating a JS function shim.
//!
//! This `Builder` type is used to generate JS function shims which sit between
//! exported functions, table elements, imports, etc. All function shims
//! generated by `wasm-bindgen` run through this type.

use crate::js::incoming;
use crate::js::outgoing;
use crate::js::Context;
use crate::webidl::Binding;
use failure::{bail, Error};
use std::collections::HashSet;
use wasm_webidl_bindings::ast;

/// A one-size-fits-all builder for processing WebIDL bindings and generating
/// JS.
pub struct Builder<'a, 'b> {
    /// Parent context used to expose helper functions and such.
    cx: &'a mut Context<'b>,
    /// Prelude JS which is present before the main invocation to prepare
    /// arguments.
    args_prelude: String,
    /// Finally block to be executed regardless of the call's status, mostly
    /// used for cleanups like free'ing.
    finally: String,
    /// Code to execute after return value is materialized.
    ret_finally: String,
    /// Argument names to the JS function shim that we're generating.
    function_args: Vec<String>,
    /// JS expressions that are arguments to the function that we're calling.
    invoc_args: Vec<String>,
    /// JS to execute just before the return value is materialized.
    ret_prelude: String,
    /// The JS expression of the actual return value.
    ret_js: String,
    /// The TypeScript definition for each argument to this function.
    pub ts_args: Vec<TypescriptArg>,
    /// The TypeScript return value for this function.
    pub ts_ret: Option<TypescriptArg>,
    /// Whether or not this is building a constructor for a Rust class, and if
    /// so what class it's constructing.
    constructor: Option<String>,
    /// Whether or not this is building a method of a Rust class instance, and
    /// whether or not the method consumes `self` or not.
    method: Option<bool>,
    /// Whether or not we're catching exceptions from the main function
    /// invocation. Currently only used for imports.
    catch: bool,
}

/// Helper struct used in incoming/outgoing to generate JS.
pub struct JsBuilder {
    typescript: Vec<TypescriptArg>,
    prelude: String,
    finally: String,
    tmp: usize,
    args: Vec<String>,
}

pub struct TypescriptArg {
    pub ty: String,
    pub name: String,
    pub optional: bool,
}

impl<'a, 'b> Builder<'a, 'b> {
    pub fn new(cx: &'a mut Context<'b>) -> Builder<'a, 'b> {
        Builder {
            cx,
            args_prelude: String::new(),
            finally: String::new(),
            ret_finally: String::new(),
            function_args: Vec::new(),
            invoc_args: Vec::new(),
            ret_prelude: String::new(),
            ret_js: String::new(),
            ts_args: Vec::new(),
            ts_ret: None,
            constructor: None,
            method: None,
            catch: false,
        }
    }

    pub fn method(&mut self, consumed: bool) {
        self.method = Some(consumed);
    }

    pub fn constructor(&mut self, class: &str) {
        self.constructor = Some(class.to_string());
    }

    pub fn catch(&mut self, catch: bool) -> Result<(), Error> {
        if catch {
            self.cx.expose_handle_error()?;
        }
        self.catch = catch;
        Ok(())
    }

    pub fn process(
        &mut self,
        binding: &Binding,
        webidl: &ast::WebidlFunction,
        incoming_args: bool,
        explicit_arg_names: &Option<Vec<String>>,
        invoke: &mut dyn FnMut(&mut Context, &mut String, &[String]) -> Result<String, Error>,
    ) -> Result<String, Error> {
        // used in `finalize` below
        if self.cx.config.debug {
            self.cx.expose_log_error();
        }

        // First up we handle all the arguments. Depending on whether incoming
        // or outgoing ar the arguments this is pretty different.
        let mut arg_names = Vec::new();
        let mut js;
        if incoming_args {
            let mut webidl_params = webidl.params.iter();

            // If we're returning via an out pointer then it's guaranteed to be
            // the first argument. This isn't an argument of the function shim
            // we're generating so synthesize the parameter and its value.
            //
            // For the actual value of the return pointer we just pick the first
            // properly aligned nonzero address. We use the address for a
            // BigInt64Array sometimes which means it needs to be 8-byte
            // aligned. Otherwise valid code is unlikely to ever be working
            // around address 8, so this should be a safe address to use for
            // returning data through.
            if binding.return_via_outptr.is_some() {
                drop(webidl_params.next());
                self.args_prelude.push_str("const retptr = 8;\n");
                arg_names.push("retptr".to_string());
            }

            // If this is a method then we're generating this as part of a class
            // method, so the leading parameter is the this pointer stored on
            // the JS object, so synthesize that here.
            match self.method {
                Some(true) => {
                    drop(webidl_params.next());
                    self.args_prelude.push_str("const ptr = this.ptr;\n");
                    self.args_prelude.push_str("this.ptr = 0;\n");
                    arg_names.push("ptr".to_string());
                }
                Some(false) => {
                    drop(webidl_params.next());
                    arg_names.push("this.ptr".to_string());
                }
                None => {}
            }

            // And now take the rest of the parameters and generate a name for them.
            for (i, _) in webidl_params.enumerate() {
                let arg = match explicit_arg_names {
                    Some(list) => list[i].clone(),
                    None => format!("arg{}", i),
                };
                self.function_args.push(arg.clone());
                arg_names.push(arg);
            }
            js = JsBuilder::new(arg_names);
            let mut args = incoming::Incoming::new(self.cx, &webidl.params, &mut js);
            for argument in binding.incoming.iter() {
                self.invoc_args.extend(args.process(argument)?);
            }
        } else {
            // If we're getting arguments from outgoing values then the ret ptr
            // is actually an argument of the function itself. That means that
            // `arg0` we generate below is the ret ptr, and we shouldn't
            // generate a JS binding for it and instead skip the first binding
            // listed.
            let mut skip = 0;
            if binding.return_via_outptr.is_some() {
                skip = 1;
            }

            // And now take the rest of the parameters and generate a name for them.
            for i in 0..self.cx.module.types.get(binding.wasm_ty).params().len() {
                let arg = format!("arg{}", i);
                self.function_args.push(arg.clone());
                arg_names.push(arg);
            }
            js = JsBuilder::new(arg_names);
            let mut args = outgoing::Outgoing::new(self.cx, &mut js);
            for argument in binding.outgoing.iter().skip(skip) {
                self.invoc_args.push(args.process(argument)?);
            }
        }

        // Save off the results of JS generation for the arguments.
        self.args_prelude.push_str(&js.prelude);
        self.finally.push_str(&js.finally);
        self.ts_args.extend(js.typescript);

        // Remove extraneous typescript args which were synthesized and aren't
        // part of our function shim.
        while self.ts_args.len() > self.function_args.len() {
            self.ts_args.remove(0);
        }

        // Handle the special case where there is no return value. In this case
        // we can skip all the logic below and go straight to the end.
        if incoming_args {
            if binding.outgoing.len() == 0 {
                assert!(binding.return_via_outptr.is_none());
                assert!(self.constructor.is_none());
                let invoc = invoke(self.cx, &mut self.args_prelude, &self.invoc_args)?;
                return Ok(self.finalize(&invoc));
            }
            assert_eq!(binding.outgoing.len(), 1);
        } else {
            if binding.incoming.len() == 0 {
                assert!(binding.return_via_outptr.is_none());
                assert!(self.constructor.is_none());
                let invoc = invoke(self.cx, &mut self.args_prelude, &self.invoc_args)?;
                return Ok(self.finalize(&invoc));
            }
            assert_eq!(binding.incoming.len(), 1);
        }

        // Like above handling the return value is quite different based on
        // whether it's an outgoing argument or an incoming argument.
        let mut ret_args = Vec::new();
        let mut js;
        if incoming_args {
            match &binding.return_via_outptr {
                // If we have an outgoing value that requires multiple
                // aggregates then we're passing a return pointer (a global one)
                // to a wasm function, and then afterwards we're going to read
                // the results of that return pointer. Here we generate an
                // expression effectively which represents reading each value of
                // the return pointer that was filled in. These values are then
                // used by the outgoing builder as inputs to generate the final
                // actual return value.
                Some(list) => {
                    let mut exposed = HashSet::new();
                    for (i, ty) in list.iter().enumerate() {
                        let (mem, size) = match ty {
                            walrus::ValType::I32 => {
                                if exposed.insert(*ty) {
                                    self.cx.expose_int32_memory();
                                    self.ret_prelude
                                        .push_str("const memi32 = getInt32Memory();\n");
                                }
                                ("memi32", 4)
                            }
                            walrus::ValType::F32 => {
                                if exposed.insert(*ty) {
                                    self.cx.expose_f32_memory();
                                    self.ret_prelude
                                        .push_str("const memf32 = getFloat32Memory();\n");
                                }
                                ("memf32", 4)
                            }
                            walrus::ValType::F64 => {
                                if exposed.insert(*ty) {
                                    self.cx.expose_f64_memory();
                                    self.ret_prelude
                                        .push_str("const memf64 = getFloat64Memory();\n");
                                }
                                ("memf64", 8)
                            }
                            _ => bail!("invalid aggregate return type"),
                        };
                        ret_args.push(format!("{}[retptr / {} + {}]", mem, size, i));
                    }
                }

                // No return pointer? That's much easier! We just have one input
                // of `ret` which is created in the JS shim below.
                None => ret_args.push("ret".to_string()),
            }
            js = JsBuilder::new(ret_args);
            let mut ret = outgoing::Outgoing::new(self.cx, &mut js);
            let ret_js = ret.process(&binding.outgoing[0])?;
            self.ret_js.push_str(&ret_js);
        } else {
            // If there's an out ptr for an incoming argument then it means that
            // the first argument to our function is the return pointer, and we
            // need to fill that in. After we process the value we then write
            // each result of the processed value into the corresponding typed
            // array.
            js = JsBuilder::new(vec!["ret".to_string()]);
            let results = match &webidl.result {
                Some(ptr) => std::slice::from_ref(ptr),
                None => &[],
            };
            let mut ret = incoming::Incoming::new(self.cx, results, &mut js);
            let ret_js = ret.process(&binding.incoming[0])?;
            match &binding.return_via_outptr {
                Some(list) => {
                    assert_eq!(list.len(), ret_js.len());
                    for (i, js) in ret_js.iter().enumerate() {
                        self.ret_finally
                            .push_str(&format!("const ret{} = {};\n", i, js));
                    }
                    for (i, ty) in list.iter().enumerate() {
                        let (mem, size) = match ty {
                            walrus::ValType::I32 => {
                                self.cx.expose_int32_memory();
                                ("getInt32Memory()", 4)
                            }
                            walrus::ValType::F32 => {
                                self.cx.expose_f32_memory();
                                ("getFloat32Memory()", 4)
                            }
                            walrus::ValType::F64 => {
                                self.cx.expose_f64_memory();
                                ("getFloat64Memory()", 8)
                            }
                            _ => bail!("invalid aggregate return type"),
                        };
                        self.ret_finally
                            .push_str(&format!("{}[arg0 / {} + {}] = ret{};\n", mem, size, i, i));
                    }
                }
                None => {
                    assert_eq!(ret_js.len(), 1);
                    self.ret_js.push_str(&ret_js[0]);
                }
            }
        }
        self.ret_finally.push_str(&js.finally);
        self.ret_prelude.push_str(&js.prelude);
        self.ts_ret = Some(js.typescript.remove(0));
        let invoc = invoke(self.cx, &mut self.args_prelude, &self.invoc_args)?;
        Ok(self.finalize(&invoc))
    }

    // This method... is a mess. Refactorings and improvements are more than
    // welcome :)
    fn finalize(&self, invoc: &str) -> String {
        let mut js = String::new();
        js.push_str("(");
        js.push_str(&self.function_args.join(", "));
        js.push_str(") {\n");
        if self.args_prelude.len() > 0 {
            js.push_str(self.args_prelude.trim());
            js.push_str("\n");
        }

        let mut call = String::new();
        if self.ts_ret.is_some() {
            call.push_str("const ret = ");
        }
        call.push_str(invoc);
        call.push_str(";\n");

        if self.ret_prelude.len() > 0 {
            call.push_str(self.ret_prelude.trim());
            call.push_str("\n");
        }

        if self.ret_js.len() > 0 {
            assert!(self.ts_ret.is_some());
            // Having a this field isn't supported yet, but shouldn't come up
            assert!(self.ret_finally.len() == 0);
            call.push_str("return ");
            call.push_str(&self.ret_js);
            call.push_str(";\n");
        } else if self.ret_finally.len() > 0 {
            call.push_str(self.ret_finally.trim());
            call.push_str("\n");
        }

        if self.catch {
            call = format!("try {{\n{}}} catch (e) {{\n handleError(e)\n}}\n", call);
        }

        // Generate a try/catch block in debug mode which handles unexpected and
        // unhandled exceptions, typically used on imports. This currently just
        // logs what happened, but keeps the exception being thrown to propagate
        // elsewhere.
        if self.cx.config.debug {
            call = format!("try {{\n{}}} catch (e) {{\n logError(e)\n}}\n", call);
        }

        let finally = self.finally.trim();
        if finally.len() != 0 {
            call = format!("try {{\n{}}} finally {{\n{}\n}}\n", call, finally);
        }

        js.push_str(&call);
        js.push_str("}");

        return js;
    }

    /// Returns the typescript signature of the binding that this has described.
    /// This is used to generate all the TypeScript definitions later on.
    ///
    /// Note that the TypeScript returned here is just the argument list and the
    /// return value, it doesn't include the function name in any way.
    pub fn typescript_signature(&self) -> String {
        // Build up the typescript signature as well
        let mut omittable = true;
        let mut ts_args = Vec::new();
        for arg in self.ts_args.iter().rev() {
            // In TypeScript, we can mark optional parameters as omittable
            // using the `?` suffix, but only if they're not followed by
            // non-omittable parameters. Therefore iterate the parameter list
            // in reverse and stop using the `?` suffix for optional params as
            // soon as a non-optional parameter is encountered.
            if arg.optional {
                if omittable {
                    ts_args.push(format!("{}?: {}", arg.name, arg.ty));
                } else {
                    ts_args.push(format!("{}: {} | undefined", arg.name, arg.ty));
                }
            } else {
                omittable = false;
                ts_args.push(format!("{}: {}", arg.name, arg.ty));
            }
        }
        ts_args.reverse();
        let mut ts = format!("({})", ts_args.join(", "));

        // Constructors have no listed return type in typescript
        if self.constructor.is_none() {
            ts.push_str(": ");
            if let Some(ty) = &self.ts_ret {
                ts.push_str(&ty.ty);
                if ty.optional {
                    ts.push_str(" | undefined");
                }
            } else {
                ts.push_str("void");
            }
        }
        return ts;
    }

    /// Returns a helpful JS doc comment which lists types for all parameters
    /// and the return value.
    pub fn js_doc_comments(&self) -> String {
        let mut ret: String = self
            .ts_args
            .iter()
            .map(|a| {
                if a.optional {
                    format!("@param {{{} | undefined}} {}\n", a.ty, a.name)
                } else {
                    format!("@param {{{}}} {}\n", a.ty, a.name)
                }
            })
            .collect();
        if let Some(ts) = &self.ts_ret {
            ret.push_str(&format!("@returns {{{}}}", ts.ty));
        }
        ret
    }
}

impl JsBuilder {
    pub fn new(args: Vec<String>) -> JsBuilder {
        JsBuilder {
            args,
            tmp: 0,
            finally: String::new(),
            prelude: String::new(),
            typescript: Vec::new(),
        }
    }

    pub fn typescript_len(&self) -> usize {
        self.typescript.len()
    }

    pub fn arg(&self, idx: u32) -> &str {
        &self.args[idx as usize]
    }

    pub fn typescript_required(&mut self, ty: &str) {
        let name = self.args[self.typescript.len()].clone();
        self.typescript.push(TypescriptArg {
            ty: ty.to_string(),
            optional: false,
            name,
        });
    }

    pub fn typescript_optional(&mut self, ty: &str) {
        let name = self.args[self.typescript.len()].clone();
        self.typescript.push(TypescriptArg {
            ty: ty.to_string(),
            optional: true,
            name,
        });
    }

    pub fn prelude(&mut self, prelude: &str) {
        for line in prelude.trim().lines() {
            self.prelude.push_str(line);
            self.prelude.push_str("\n");
        }
    }

    pub fn finally(&mut self, finally: &str) {
        for line in finally.trim().lines() {
            self.finally.push_str(line);
            self.finally.push_str("\n");
        }
    }

    pub fn tmp(&mut self) -> usize {
        let ret = self.tmp;
        self.tmp += 1;
        return ret;
    }
}
