#[cfg(feature = "jit")]
mod jitfunc;

use super::{
    tuple::PyTupleTyped, PyAsyncGen, PyCode, PyCoroutine, PyDictRef, PyGenerator, PyStr, PyStrRef,
    PyTupleRef, PyType, PyTypeRef,
};
#[cfg(feature = "jit")]
use crate::common::lock::OnceCell;
use crate::common::lock::PyMutex;
use crate::convert::ToPyObject;
use crate::function::ArgMapping;
use crate::object::{Traverse, TraverseFn};
use crate::{
    bytecode,
    class::PyClassImpl,
    frame::Frame,
    function::{FuncArgs, OptionalArg, PyComparisonValue, PySetterValue},
    scope::Scope,
    types::{
        Callable, Comparable, Constructor, GetAttr, GetDescriptor, PyComparisonOp, Representable,
    },
    AsObject, Context, Py, PyObject, PyObjectRef, PyPayload, PyRef, PyResult, VirtualMachine,
};
use itertools::Itertools;
#[cfg(feature = "jit")]
use rustpython_jit::CompiledCode;

#[pyclass(module = false, name = "function", traverse = "manual")]
#[derive(Debug)]
pub struct PyFunction {
    code: PyRef<PyCode>,
    globals: PyDictRef,
    closure: Option<PyTupleTyped<PyCellRef>>,
    defaults_and_kwdefaults: PyMutex<(Option<PyTupleRef>, Option<PyDictRef>)>,
    name: PyMutex<PyStrRef>,
    qualname: PyMutex<PyStrRef>,
    type_params: PyMutex<PyTupleRef>,
    #[cfg(feature = "jit")]
    jitted_code: OnceCell<CompiledCode>,
    annotations: PyMutex<PyDictRef>,
    module: PyMutex<PyObjectRef>,
    doc: PyMutex<PyObjectRef>,
}

unsafe impl Traverse for PyFunction {
    fn traverse(&self, tracer_fn: &mut TraverseFn) {
        self.globals.traverse(tracer_fn);
        self.closure.traverse(tracer_fn);
        self.defaults_and_kwdefaults.traverse(tracer_fn);
    }
}

impl PyFunction {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        code: PyRef<PyCode>,
        globals: PyDictRef,
        closure: Option<PyTupleTyped<PyCellRef>>,
        defaults: Option<PyTupleRef>,
        kw_only_defaults: Option<PyDictRef>,
        qualname: PyStrRef,
        type_params: PyTupleRef,
        annotations: PyDictRef,
        module: PyObjectRef,
        doc: PyObjectRef,
    ) -> Self {
        let name = PyMutex::new(code.obj_name.to_owned());
        PyFunction {
            code,
            globals,
            closure,
            defaults_and_kwdefaults: PyMutex::new((defaults, kw_only_defaults)),
            name,
            qualname: PyMutex::new(qualname),
            type_params: PyMutex::new(type_params),
            #[cfg(feature = "jit")]
            jitted_code: OnceCell::new(),
            annotations: PyMutex::new(annotations),
            module: PyMutex::new(module),
            doc: PyMutex::new(doc),
        }
    }

    fn fill_locals_from_args(
        &self,
        frame: &Frame,
        func_args: FuncArgs,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        let code = &*self.code;
        let nargs = func_args.args.len();
        let nexpected_args = code.arg_count as usize;
        let total_args = code.arg_count as usize + code.kwonlyarg_count as usize;
        // let arg_names = self.code.arg_names();

        // This parses the arguments from args and kwargs into
        // the proper variables keeping into account default values
        // and starargs and kwargs.
        // See also: PyEval_EvalCodeWithName in cpython:
        // https://github.com/python/cpython/blob/main/Python/ceval.c#L3681

        let mut fastlocals = frame.fastlocals.lock();

        let mut args_iter = func_args.args.into_iter();

        // Copy positional arguments into local variables
        // zip short-circuits if either iterator returns None, which is the behavior we want --
        // only fill as much as there is to fill with as much as we have
        for (local, arg) in Iterator::zip(
            fastlocals.iter_mut().take(nexpected_args),
            args_iter.by_ref().take(nargs),
        ) {
            *local = Some(arg);
        }

        let mut vararg_offset = total_args;
        // Pack other positional arguments in to *args:
        if code.flags.contains(bytecode::CodeFlags::HAS_VARARGS) {
            let vararg_value = vm.ctx.new_tuple(args_iter.collect());
            fastlocals[vararg_offset] = Some(vararg_value.into());
            vararg_offset += 1;
        } else {
            // Check the number of positional arguments
            if nargs > nexpected_args {
                return Err(vm.new_type_error(format!(
                    "{}() takes {} positional arguments but {} were given",
                    self.qualname(),
                    nexpected_args,
                    nargs
                )));
            }
        }

        // Do we support `**kwargs` ?
        let kwargs = if code.flags.contains(bytecode::CodeFlags::HAS_VARKEYWORDS) {
            let d = vm.ctx.new_dict();
            fastlocals[vararg_offset] = Some(d.clone().into());
            Some(d)
        } else {
            None
        };

        let argpos = |range: std::ops::Range<_>, name: &str| {
            code.varnames
                .iter()
                .enumerate()
                .skip(range.start)
                .take(range.end - range.start)
                .find(|(_, s)| s.as_str() == name)
                .map(|(p, _)| p)
        };

        let mut posonly_passed_as_kwarg = Vec::new();
        // Handle keyword arguments
        for (name, value) in func_args.kwargs {
            // Check if we have a parameter with this name:
            if let Some(pos) = argpos(code.posonlyarg_count as usize..total_args, &name) {
                let slot = &mut fastlocals[pos];
                if slot.is_some() {
                    return Err(vm.new_type_error(format!(
                        "{}() got multiple values for argument '{}'",
                        self.qualname(),
                        name
                    )));
                }
                *slot = Some(value);
            } else if let Some(kwargs) = kwargs.as_ref() {
                kwargs.set_item(&name, value, vm)?;
            } else if argpos(0..code.posonlyarg_count as usize, &name).is_some() {
                posonly_passed_as_kwarg.push(name);
            } else {
                return Err(vm.new_type_error(format!(
                    "{}() got an unexpected keyword argument '{}'",
                    self.qualname(),
                    name
                )));
            }
        }
        if !posonly_passed_as_kwarg.is_empty() {
            return Err(vm.new_type_error(format!(
                "{}() got some positional-only arguments passed as keyword arguments: '{}'",
                self.qualname(),
                posonly_passed_as_kwarg.into_iter().format(", "),
            )));
        }

        let mut defaults_and_kwdefaults = None;
        // can't be a closure cause it returns a reference to a captured variable :/
        macro_rules! get_defaults {
            () => {{
                defaults_and_kwdefaults
                    .get_or_insert_with(|| self.defaults_and_kwdefaults.lock().clone())
            }};
        }

        // Add missing positional arguments, if we have fewer positional arguments than the
        // function definition calls for
        if nargs < nexpected_args {
            let defaults = get_defaults!().0.as_ref().map(|tup| tup.as_slice());
            let ndefs = defaults.map_or(0, |d| d.len());

            let nrequired = code.arg_count as usize - ndefs;

            // Given the number of defaults available, check all the arguments for which we
            // _don't_ have defaults; if any are missing, raise an exception
            let mut missing: Vec<_> = (nargs..nrequired)
                .filter_map(|i| {
                    if fastlocals[i].is_none() {
                        Some(&code.varnames[i])
                    } else {
                        None
                    }
                })
                .collect();
            let missing_args_len = missing.len();

            if !missing.is_empty() {
                let last = if missing.len() > 1 {
                    missing.pop()
                } else {
                    None
                };

                let (and, right) = if let Some(last) = last {
                    (
                        if missing.len() == 1 {
                            "' and '"
                        } else {
                            "', and '"
                        },
                        last.as_str(),
                    )
                } else {
                    ("", "")
                };

                return Err(vm.new_type_error(format!(
                    "{}() missing {} required positional argument{}: '{}{}{}'",
                    self.qualname(),
                    missing_args_len,
                    if missing_args_len == 1 { "" } else { "s" },
                    missing.iter().join("', '"),
                    and,
                    right,
                )));
            }

            if let Some(defaults) = defaults {
                let n = std::cmp::min(nargs, nexpected_args);
                let i = n.saturating_sub(nrequired);

                // We have sufficient defaults, so iterate over the corresponding names and use
                // the default if we don't already have a value
                for i in i..defaults.len() {
                    let slot = &mut fastlocals[nrequired + i];
                    if slot.is_none() {
                        *slot = Some(defaults[i].clone());
                    }
                }
            }
        };

        if code.kwonlyarg_count > 0 {
            // TODO: compile a list of missing arguments
            // let mut missing = vec![];
            // Check if kw only arguments are all present:
            for (slot, kwarg) in fastlocals
                .iter_mut()
                .zip(&*code.varnames)
                .skip(code.arg_count as usize)
                .take(code.kwonlyarg_count as usize)
                .filter(|(slot, _)| slot.is_none())
            {
                if let Some(defaults) = &get_defaults!().1 {
                    if let Some(default) = defaults.get_item_opt(&**kwarg, vm)? {
                        *slot = Some(default);
                        continue;
                    }
                }

                // No default value and not specified.
                return Err(
                    vm.new_type_error(format!("Missing required kw only argument: '{kwarg}'"))
                );
            }
        }

        if let Some(cell2arg) = code.cell2arg.as_deref() {
            for (cell_idx, arg_idx) in cell2arg.iter().enumerate().filter(|(_, i)| **i != -1) {
                let x = fastlocals[*arg_idx as usize].take();
                frame.cells_frees[cell_idx].set(x);
            }
        }

        Ok(())
    }

    pub async fn invoke_with_locals(
        &self,
        func_args: FuncArgs,
        locals: Option<ArgMapping>,
        vm: &VirtualMachine,
    ) -> PyResult {
        #[cfg(feature = "jit")]
        if let Some(jitted_code) = self.jitted_code.get() {
            match jitfunc::get_jit_args(self, &func_args, jitted_code, vm) {
                Ok(args) => {
                    return Ok(args.invoke().to_pyobject(vm));
                }
                Err(err) => info!(
                    "jit: function `{}` is falling back to being interpreted because of the \
                    error: {}",
                    self.code.obj_name, err
                ),
            }
        }

        let code = &self.code;

        let locals = if self.code.flags.contains(bytecode::CodeFlags::NEW_LOCALS) {
            ArgMapping::from_dict_exact(vm.ctx.new_dict())
        } else if let Some(locals) = locals {
            locals
        } else {
            ArgMapping::from_dict_exact(self.globals.clone())
        };

        // Construct frame:
        let frame = Frame::new(
            code.clone(),
            Scope::new(Some(locals), self.globals.clone()),
            vm.builtins.dict(),
            self.closure.as_ref().map_or(&[], |c| c.as_slice()),
            vm,
        )
        .into_ref(&vm.ctx);

        self.fill_locals_from_args(&frame, func_args, vm)?;

        // If we have a generator, create a new generator
        let is_gen = code.flags.contains(bytecode::CodeFlags::IS_GENERATOR);
        let is_coro = code.flags.contains(bytecode::CodeFlags::IS_COROUTINE);
        match (is_gen, is_coro) {
            (true, false) => Ok(PyGenerator::new(frame, self.name()).into_pyobject(vm)),
            (false, true) => Ok(PyCoroutine::new(frame, self.name()).into_pyobject(vm)),
            (true, true) => Ok(PyAsyncGen::new(frame, self.name()).into_pyobject(vm)),
            (false, false) => vm.run_frame(frame).await,
        }
    }

    #[inline(always)]
    pub async fn invoke(&self, func_args: FuncArgs, vm: &VirtualMachine) -> PyResult {
        self.invoke_with_locals(func_args, None, vm).await
    }
}

impl PyPayload for PyFunction {
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.function_type
    }
}

#[pyclass(
    with(GetDescriptor, Callable, Representable),
    flags(HAS_DICT, METHOD_DESCRIPTOR)
)]
impl PyFunction {
    #[pygetset(magic)]
    fn code(&self) -> PyRef<PyCode> {
        self.code.clone()
    }

    #[pygetset(magic)]
    fn defaults(&self) -> Option<PyTupleRef> {
        self.defaults_and_kwdefaults.lock().0.clone()
    }
    #[pygetset(magic, setter)]
    fn set_defaults(&self, defaults: Option<PyTupleRef>) {
        self.defaults_and_kwdefaults.lock().0 = defaults
    }

    #[pygetset(magic)]
    fn kwdefaults(&self) -> Option<PyDictRef> {
        self.defaults_and_kwdefaults.lock().1.clone()
    }
    #[pygetset(magic, setter)]
    fn set_kwdefaults(&self, kwdefaults: Option<PyDictRef>) {
        self.defaults_and_kwdefaults.lock().1 = kwdefaults
    }

    // {"__closure__",   T_OBJECT,     OFF(func_closure), READONLY},
    // {"__doc__",       T_OBJECT,     OFF(func_doc), 0},
    // {"__globals__",   T_OBJECT,     OFF(func_globals), READONLY},
    // {"__module__",    T_OBJECT,     OFF(func_module), 0},
    // {"__builtins__",  T_OBJECT,     OFF(func_builtins), READONLY},
    #[pymember(magic)]
    fn globals(vm: &VirtualMachine, zelf: PyObjectRef) -> PyResult {
        let zelf = Self::_as_pyref(&zelf, vm)?;
        Ok(zelf.globals.clone().into())
    }

    #[pymember(magic)]
    fn closure(vm: &VirtualMachine, zelf: PyObjectRef) -> PyResult {
        let zelf = Self::_as_pyref(&zelf, vm)?;
        Ok(vm.unwrap_or_none(zelf.closure.clone().map(|x| x.to_pyobject(vm))))
    }

    #[pygetset(magic)]
    fn name(&self) -> PyStrRef {
        self.name.lock().clone()
    }

    #[pygetset(magic, setter)]
    fn set_name(&self, name: PyStrRef) {
        *self.name.lock() = name;
    }

    #[pymember(magic)]
    fn doc(_vm: &VirtualMachine, zelf: PyObjectRef) -> PyResult {
        let zelf: PyRef<PyFunction> = zelf.downcast().unwrap_or_else(|_| unreachable!());
        let doc = zelf.doc.lock();
        Ok(doc.clone())
    }

    #[pymember(magic, setter)]
    fn set_doc(vm: &VirtualMachine, zelf: PyObjectRef, value: PySetterValue) -> PyResult<()> {
        let zelf: PyRef<PyFunction> = zelf.downcast().unwrap_or_else(|_| unreachable!());
        let value = value.unwrap_or_none(vm);
        *zelf.doc.lock() = value;
        Ok(())
    }

    #[pygetset(magic)]
    fn module(&self) -> PyObjectRef {
        self.module.lock().clone()
    }

    #[pygetset(magic, setter)]
    fn set_module(&self, module: PySetterValue<PyObjectRef>, vm: &VirtualMachine) {
        *self.module.lock() = module.unwrap_or_none(vm);
    }

    #[pygetset(magic)]
    fn annotations(&self) -> PyDictRef {
        self.annotations.lock().clone()
    }

    #[pygetset(magic, setter)]
    fn set_annotations(&self, annotations: PyDictRef) {
        *self.annotations.lock() = annotations
    }

    #[pygetset(magic)]
    fn qualname(&self) -> PyStrRef {
        self.qualname.lock().clone()
    }

    #[pygetset(magic, setter)]
    fn set_qualname(&self, value: PySetterValue, vm: &VirtualMachine) -> PyResult<()> {
        match value {
            PySetterValue::Assign(value) => {
                let Ok(qualname) = value.downcast::<PyStr>() else {
                    return Err(vm.new_type_error(
                        "__qualname__ must be set to a string object".to_string(),
                    ));
                };
                *self.qualname.lock() = qualname;
            }
            PySetterValue::Delete => {
                return Err(
                    vm.new_type_error("__qualname__ must be set to a string object".to_string())
                );
            }
        }
        Ok(())
    }

    #[pygetset(magic)]
    fn type_params(&self) -> PyTupleRef {
        self.type_params.lock().clone()
    }

    #[pygetset(magic, setter)]
    fn set_type_params(
        &self,
        value: PySetterValue<PyTupleRef>,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        match value {
            PySetterValue::Assign(value) => {
                *self.type_params.lock() = value;
            }
            PySetterValue::Delete => {
                return Err(
                    vm.new_type_error("__type_params__ must be set to a tuple object".to_string())
                );
            }
        }
        Ok(())
    }

    #[cfg(feature = "jit")]
    #[pymethod(magic)]
    fn jit(zelf: PyRef<Self>, vm: &VirtualMachine) -> PyResult<()> {
        zelf.jitted_code
            .get_or_try_init(|| {
                let arg_types = jitfunc::get_jit_arg_types(&zelf, vm)?;
                rustpython_jit::compile(&zelf.code.code, &arg_types)
                    .map_err(|err| jitfunc::new_jit_error(err.to_string(), vm))
            })
            .map(drop)
    }
}

impl GetDescriptor for PyFunction {
    fn descr_get(
        zelf: PyObjectRef,
        obj: Option<PyObjectRef>,
        cls: Option<PyObjectRef>,
        vm: &VirtualMachine,
    ) -> PyResult {
        let (_zelf, obj) = Self::_unwrap(&zelf, obj, vm)?;
        let obj = if vm.is_none(&obj) && !Self::_cls_is(&cls, obj.class()) {
            zelf
        } else {
            PyBoundMethod::new_ref(obj, zelf, &vm.ctx).into()
        };
        Ok(obj)
    }
}

impl Callable for PyFunction {
    type Args = FuncArgs;
    #[inline]
    async fn call(zelf: &Py<Self>, args: FuncArgs, vm: &VirtualMachine) -> PyResult {
        zelf.invoke(args, vm).await
    }
}

impl Representable for PyFunction {
    #[inline]
    fn repr_str(zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
        Ok(format!(
            "<function {} at {:#x}>",
            zelf.qualname(),
            zelf.get_id()
        ))
    }
}

#[pyclass(module = false, name = "method", traverse)]
#[derive(Debug)]
pub struct PyBoundMethod {
    object: PyObjectRef,
    function: PyObjectRef,
}

impl Callable for PyBoundMethod {
    type Args = FuncArgs;
    #[inline]
    async fn call(zelf: &Py<Self>, mut args: FuncArgs, vm: &VirtualMachine) -> PyResult {
        args.prepend_arg(zelf.object.clone());
        zelf.function.call(args, vm)
    }
}

impl Comparable for PyBoundMethod {
    fn cmp(
        zelf: &Py<Self>,
        other: &PyObject,
        op: PyComparisonOp,
        _vm: &VirtualMachine,
    ) -> PyResult<PyComparisonValue> {
        op.eq_only(|| {
            let other = class_or_notimplemented!(Self, other);
            Ok(PyComparisonValue::Implemented(
                zelf.function.is(&other.function) && zelf.object.is(&other.object),
            ))
        })
    }
}

impl GetAttr for PyBoundMethod {
    fn getattro(zelf: &Py<Self>, name: &Py<PyStr>, vm: &VirtualMachine) -> PyResult {
        let class_attr = vm
            .ctx
            .interned_str(name)
            .and_then(|attr_name| zelf.get_class_attr(attr_name));
        if let Some(obj) = class_attr {
            return vm.call_if_get_descriptor(&obj, zelf.to_owned().into());
        }
        zelf.function.get_attr(name, vm)
    }
}

#[derive(FromArgs)]
pub struct PyBoundMethodNewArgs {
    #[pyarg(positional)]
    function: PyObjectRef,
    #[pyarg(positional)]
    object: PyObjectRef,
}

impl Constructor for PyBoundMethod {
    type Args = PyBoundMethodNewArgs;

    fn py_new(
        cls: PyTypeRef,
        Self::Args { function, object }: Self::Args,
        vm: &VirtualMachine,
    ) -> PyResult {
        PyBoundMethod::new(object, function)
            .into_ref_with_type(vm, cls)
            .map(Into::into)
    }
}

impl PyBoundMethod {
    fn new(object: PyObjectRef, function: PyObjectRef) -> Self {
        PyBoundMethod { object, function }
    }

    pub fn new_ref(object: PyObjectRef, function: PyObjectRef, ctx: &Context) -> PyRef<Self> {
        PyRef::new_ref(
            Self::new(object, function),
            ctx.types.bound_method_type.to_owned(),
            None,
        )
    }
}

#[pyclass(
    with(Callable, Comparable, GetAttr, Constructor, Representable),
    flags(HAS_DICT)
)]
impl PyBoundMethod {
    #[pymethod(magic)]
    fn reduce(
        &self,
        vm: &VirtualMachine,
    ) -> (Option<PyObjectRef>, (PyObjectRef, Option<PyObjectRef>)) {
        let builtins_getattr = vm.builtins.get_attr("getattr", vm).ok();
        let funcself = self.object.clone();
        let funcname = self.function.get_attr("__name__", vm).ok();
        (builtins_getattr, (funcself, funcname))
    }

    #[pygetset(magic)]
    fn doc(&self, vm: &VirtualMachine) -> PyResult {
        self.function.get_attr("__doc__", vm)
    }

    #[pygetset(magic)]
    fn func(&self) -> PyObjectRef {
        self.function.clone()
    }

    #[pygetset(name = "__self__")]
    fn get_self(&self) -> PyObjectRef {
        self.object.clone()
    }

    #[pygetset(magic)]
    fn module(&self, vm: &VirtualMachine) -> Option<PyObjectRef> {
        self.function.get_attr("__module__", vm).ok()
    }

    #[pygetset(magic)]
    fn qualname(&self, vm: &VirtualMachine) -> PyResult {
        if self
            .function
            .fast_isinstance(vm.ctx.types.builtin_function_or_method_type)
        {
            // Special case: we work with `__new__`, which is not really a method.
            // It is a function, so its `__qualname__` is just `__new__`.
            // We need to add object's part manually.
            let obj_name = vm.get_attribute_opt(self.object.clone(), "__qualname__")?;
            let obj_name: Option<PyStrRef> = obj_name.and_then(|o| o.downcast().ok());
            return Ok(vm
                .ctx
                .new_str(format!(
                    "{}.__new__",
                    obj_name.as_ref().map_or("?", |s| s.as_str())
                ))
                .into());
        }
        self.function.get_attr("__qualname__", vm)
    }
}

impl PyPayload for PyBoundMethod {
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.bound_method_type
    }
}

impl Representable for PyBoundMethod {
    #[inline]
    fn repr_str(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<String> {
        #[allow(clippy::needless_match)] // False positive on nightly
        let funcname =
            if let Some(qname) = vm.get_attribute_opt(zelf.function.clone(), "__qualname__")? {
                Some(qname)
            } else {
                vm.get_attribute_opt(zelf.function.clone(), "__name__")?
            };
        let funcname: Option<PyStrRef> = funcname.and_then(|o| o.downcast().ok());
        Ok(format!(
            "<bound method {} of {}>",
            funcname.as_ref().map_or("?", |s| s.as_str()),
            &zelf.object.repr(vm)?.as_str(),
        ))
    }
}

#[pyclass(module = false, name = "cell", traverse)]
#[derive(Debug, Default)]
pub(crate) struct PyCell {
    contents: PyMutex<Option<PyObjectRef>>,
}
pub(crate) type PyCellRef = PyRef<PyCell>;

impl PyPayload for PyCell {
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.cell_type
    }
}

impl Constructor for PyCell {
    type Args = OptionalArg;

    fn py_new(cls: PyTypeRef, value: Self::Args, vm: &VirtualMachine) -> PyResult {
        Self::new(value.into_option())
            .into_ref_with_type(vm, cls)
            .map(Into::into)
    }
}

#[pyclass(with(Constructor))]
impl PyCell {
    pub fn new(contents: Option<PyObjectRef>) -> Self {
        Self {
            contents: PyMutex::new(contents),
        }
    }

    pub fn get(&self) -> Option<PyObjectRef> {
        self.contents.lock().clone()
    }
    pub fn set(&self, x: Option<PyObjectRef>) {
        *self.contents.lock() = x;
    }

    #[pygetset]
    fn cell_contents(&self, vm: &VirtualMachine) -> PyResult {
        self.get()
            .ok_or_else(|| vm.new_value_error("Cell is empty".to_owned()))
    }
    #[pygetset(setter)]
    fn set_cell_contents(&self, x: PyObjectRef) {
        self.set(Some(x))
    }
}

pub fn init(context: &Context) {
    PyFunction::extend_class(context, context.types.function_type);
    PyBoundMethod::extend_class(context, context.types.bound_method_type);
    PyCell::extend_class(context, context.types.cell_type);
}
