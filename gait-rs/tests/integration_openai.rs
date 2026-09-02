use futures_util::StreamExt;
use gait_core::provider::StreamChunk;
use gait_core::{load_provider, Message, MessageContent};

#[tokio::test]
async fn openai_streams_hello() {
    let key = match std::env::var("OPENAI_API_KEY") {
        Ok(k) => k,
        Err(_) => {
            eprintln!("OPENAI_API_KEY not set, skipping");
            return;
        }
    };

    let provider = load_provider("openai/gpt-4o-mini", Some(key)).unwrap();
    let messages = vec![Message::User {
        id: String::new(),
        content: MessageContent::Text("Say exactly: hello world".into()),
        attribution: Vec::new(),
    }];

    let mut stream = provider
        .stream(
            "You are helpful.",
            &messages,
            &[],
            &Default::default(),
            None,
        )
        .await
        .unwrap();
    let mut text = String::new();
    let mut output_tokens = 0u32;
    while let Some(chunk) = stream.next().await {
        match chunk.unwrap() {
            StreamChunk::Text(t) => text.push_str(&t),
            StreamChunk::StepUsage(u) => output_tokens += u.output_tokens,
            StreamChunk::Done => break,
            _ => {}
        }
    }
    assert!(text.to_lowercase().contains("hello"));
    assert!(output_tokens > 0);
}
