use futures_util::StreamExt;
use gait_core::provider::StreamChunk;
use gait_core::{load_provider, Message, MessageContent};

#[tokio::test]
async fn anthropic_streams_hello() {
    let key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("ANTHROPIC_API_KEY not set, skipping");
            return;
        }
    };

    let provider = load_provider("anthropic/claude-haiku-4-5", Some(key)).unwrap();
    let messages = vec![Message::User {
        id: String::new(),
        content: MessageContent::Text("Say exactly: hello world".into()),
    }];

    let mut stream = provider
        .stream("You are helpful.", &messages, &[], &Default::default(), None)
        .await
        .unwrap();
    let mut text = String::new();
    let mut output_tokens = 0u32;
    let mut input_tokens = 0u32;
    while let Some(chunk) = stream.next().await {
        match chunk.unwrap() {
            StreamChunk::Text(t) => text.push_str(&t),
            StreamChunk::StepUsage(u) => {
                output_tokens += u.output_tokens;
                input_tokens += u.input_tokens;
            }
            StreamChunk::Done => break,
            _ => {}
        }
    }
    assert!(text.to_lowercase().contains("hello"));
    assert!(output_tokens > 0);
    // Input tokens tracked from MessageStart (C5 fix)
    assert!(input_tokens > 0);
}
