use gait_ai::{load_provider, stream_text, Message, MessageContent};

#[tokio::test]
async fn openai_streams_hello() {
    let key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) => k,
        Err(_) => { eprintln!("OPENAI_API_KEY not set, skipping"); return; }
    };

    let provider = load_provider("openai/gpt-4o-mini", Some(key)).unwrap();
    let messages = vec![Message::User {
        content: MessageContent::Text("Say exactly: hello world".into()),
    }];

    let result = stream_text(&*provider, "You are helpful.", &messages, &[]).await.unwrap();
    assert!(result.text.to_lowercase().contains("hello"));
    assert!(result.usage.output_tokens > 0);
}
