fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let report = devspace_kernel_oracle::generate(true)?;
    println!(
        "walked {} objects ({} commits, {} trees, {} blobs); wrote {} curated vectors; op goldens: {} ported, {} regenerated views, {} regenerated operations",
        report.walked,
        report.commits,
        report.trees,
        report.blobs,
        report.curated,
        report.op_ported,
        report.op_regenerated_views,
        report.op_regenerated_operations,
    );
    Ok(())
}
