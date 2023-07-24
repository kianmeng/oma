use oma_pm::apt::{AptArgs, OmaApt, OmaAptError};

fn main() -> Result<(), OmaAptError> {
    let apt = OmaApt::new()?;
    let pkgs = apt.select_pkg(vec!["fish"])?;

    apt.install(pkgs, false)?;
    let op = apt.operation_vec()?;
    dbg!(op);

    apt.commit(None, AptArgs::default())?;

    Ok(())
}