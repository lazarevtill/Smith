fn main() -> Result<(), Box<dyn std::error::Error>> {
    let report = smith_agent_core::fixture::run_deterministic_fixture()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
