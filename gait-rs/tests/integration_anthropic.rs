use gait_ai::{load_provider, stream_text, Message, MessageContent};

#[tokio::test]
async fn anthropic_streams_hello() {
    let key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) => k,
        Err(_) => { eprintln!("ANTHROPIC_API_KEY not set, skipping"); return; }
    };

    let provider = load_provider("anthropic/claude-haiku-4-5", Some(key)).unwrap();
    let messages = vec![Message::User {
        content: MessageContent::Text("Say exactly: hello world".into()),
    }];

    let result = stream_text(&*provider, "You are helpful.", &messages, &[]).await.unwrap();
    assert!(result.text.to_lowercase().contains("hello"));
    assert!(result.usage.output_tokens > 0);
    // Input tokens now tracked from MessageStart (C5 fix)
    assert!(result.usage.input_tokens > 0);
}
