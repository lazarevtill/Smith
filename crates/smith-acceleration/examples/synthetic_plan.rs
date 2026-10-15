#[path = "support/fixture.rs"]
mod fixture;
fn main() {
    let (inventory, adapters, model, intent) = fixture::fixture();
    let output = smith_acceleration::plan(&inventory, &adapters, &model, &intent, 7).unwrap();
    println!("{}",serde_json::to_string_pretty(&serde_json::json!({
        "synthetic_only":true,"inventory":inventory,"adapters":adapters,"model":model,"intent":intent,"outcome":output
    })).unwrap());
}
