#[test]
fn debug_user_file_parse() {
    use latte_agent_core::config::ModelDef;
    let toml = std::fs::read_to_string("/home/dong/.latte/models.d/deepseek.toml").unwrap();
    let r: Result<Vec<ModelDef>, _> = toml::from_str(&toml);
    match r {
        Ok(v) => println!("OK count={}", v.len()),
        Err(e) => panic!("ERR: {}", e),
    }
}
