//! Loaded images: `images`.

use super::{CommandResult, Flow, ReplCommand, no_args};
use crate::repl::Ctx;

/// `images`: the main executable and libraries with their load addresses.
pub struct Images;

impl ReplCommand for Images
{
    fn names(&self) -> &'static [&'static str]
    {
        &["images"]
    }

    fn summary(&self) -> &'static str
    {
        "list the loaded executable and libraries with their load addresses"
    }

    fn help(&self) -> &'static str
    {
        "images\n\nList each image the loader reports as '<load address> <path>', the main executable first \
         when the loader reports it first. dyld is listed too, though it leaves itself out of the list it \
         publishes. The load address includes the ASLR slide."
    }

    fn run(&self, ctx: &mut Ctx<'_>, args: &str) -> CommandResult
    {
        no_args("images", args)?;
        for image in ctx.session().images()? {
            writeln!(ctx.out, "{:#x} {}", image.load_address.value(), image.path.display())?;
        }
        Ok(Flow::Continue)
    }
}
