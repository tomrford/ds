fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let report = devspace_kernel_git_oracle::generate(true)?;
    println!(
        "walked {} objects ({} commits, {} trees, {} blobs); wrote {} curated vectors",
        report.walked, report.commits, report.trees, report.blobs, report.curated
    );
    Ok(())
}
