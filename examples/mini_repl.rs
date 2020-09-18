///! This example show cases a very simple REPL.
///! While a much better REPL can be found in ../src/shell,
///! This much smaller REPL is still a useful example because it showcases inserting
///! values and functions into the Python runtime's scope, and showcases use
///! of the compilation mode "Single".
use rustpython_compiler as compiler;
use rustpython_vm as vm;
// these are needed for special memory shenanigans to let us share a variable with Python and Rust
use std::sync::atomic::{AtomicBool, Ordering};
// this needs to be in scope in order to insert things into scope.globals
use vm::pyobject::ItemProtocol;

// This has to be a macro because it uses the py_compile macro,
// which compiles python source to optimized bytecode at compile time, so that
// the program you're embedding this into doesn't take longer to start up.
macro_rules! add_python_function {
    ( $scope:ident, $vm:ident, $src:literal $(,)? ) => {{
        // this has to be in scope to turn a PyValue into a PyRef
        // (a PyRef is a special reference that points to something in the VirtualMachine)
        use vm::pyobject::PyValue;

        let code = vm::py_compile!(source = $src);

        // takes the first constant in the file that's a function
        let def = code
            .get_constants()
            .find_map(|c| match c {
                vm::bytecode::Constant::Code { code } => Some(code),
                _ => None,
            })
            .expect("No functions found in the provided module!");

        // inserts the first function found in the module into the provided scope.
        $scope.globals.set_item(
            def.obj_name.as_str(),
            $vm.ctx.new_pyfunction(
                vm::obj::objcode::PyCode::new(*def.clone()).into_ref(&$vm),
                $scope.clone(),
                None,
                None,
            ),
            &$vm,
        )
    }};
}

static ON: AtomicBool = AtomicBool::new(false);

fn on(b: bool) {
    ON.store(b, Ordering::Relaxed);
}

fn main() -> vm::pyobject::PyResult<()> {
    vm::Interpreter::default().enter(run)
}

fn run(vm: &vm::VirtualMachine) -> vm::pyobject::PyResult<()> {
    let mut input = String::with_capacity(50);
    let stdin = std::io::stdin();

    let scope: vm::scope::Scope = vm.new_scope_with_builtins();

    // typing `quit()` is too long, let's make `on(False)` work instead.
    scope.globals.set_item("on", vm.ctx.new_function(on), vm)?;

    // let's include a fibonacci function, but let's be lazy and write it in Python
    add_python_function!(
        scope,
        vm,
        // a fun line to test this with is
        // ''.join( l * fib(i) for i, l in enumerate('supercalifragilistic') )
        r#"def fib(n): return n if n <= 1 else fib(n - 1) + fib(n - 2)"#
    )?;

    while ON.load(Ordering::Relaxed) {
        input.clear();
        stdin
            .read_line(&mut input)
            .expect("Failed to read line of input");

        // this line also automatically prints the output
        // (note that this is only the case when compile::Mode::Single is passed to vm.compile)
        match vm
            .compile(
                &input,
                compiler::compile::Mode::Single,
                "<embedded>".to_owned(),
            )
            .map_err(|err| vm.new_syntax_error(&err))
            .and_then(|code_obj| vm.run_code_obj(code_obj, scope.clone()))
        {
            Ok(output) => {
                // store the last value in the "last" variable
                if !vm.is_none(&output) {
                    scope.globals.set_item("last", output, vm)?;
                }
            }
            Err(e) => {
                vm::exceptions::print_exception(vm, e);
            }
        }
    }

    Ok(())
}
