/*
 * Includes
 */
use std::convert::Infallible;
use std::collections::HashMap;
use serde::{Serialize, Deserialize};
use tracing_subscriber::fmt::format::FmtSpan;
use warp::Filter;
use lapin;
use twiml;
use twiml::Twiml;

const RABBITMQ_ADDR: &str = "amqp://127.0.0.1:5672/%2f";

#[derive(Serialize, Deserialize, Clone)]
struct RabbitIncomingMessage {
    account_sid: String,
    call_sid: String
}

#[derive(Serialize, Deserialize, Clone)]
struct RabbitResponseMessage {
    content: String
}

/*
 * Main
 */
#[tokio::main]
async fn main() {
    enable_logs();
    match setup_rabbit_queue().await {
        Ok(()) => println!("Queue initialised"),
        Err(_) => {
            println!("Queue initialisation failed");
            std::process::exit(1)
        }
    };

    let incoming = warp::path("incoming")
        .and(warp::post())
        .and(warp::body::form())
        .and_then(|body| twiml_response(body))
        .with(warp::trace::request());

    warp::serve(incoming).run(([127,0,0,1], 3000)).await;
}


/*
 * Generate the TwiML response by putting a message on the queue,
 * adding a new queue to rabbit based on call_sid name and
 * wait on the response from the BE.
 */
async fn twiml_response(body: HashMap<String,String>) -> Result<impl warp::Reply, Infallible> {
    let account_sid = body.get("AccountSid");
    let call_sid = body.get("CallSid");

    let voice_content: String = match (account_sid, call_sid) {
        (None, None) => "Call failed, details unavailable".into(),
        (Some(_), None) => "Call failed, only found Account SID".into(),
        (None, Some(_)) => "Call failed, only found Call SID".into(),
        (Some(account_sid), Some(call_sid)) => {
            let incoming_config = RabbitIncomingMessage {
                account_sid: account_sid.clone(),
                call_sid: call_sid.clone()
            };
             match response_from_be_request_queue(incoming_config).await {
                 Ok(response) => response,
                 Err(_) => "An error occurred on the backend somewhere.".into() // handle the error by sending to sentry
             }
        }
    };

    let welcome_intent: String =
        twiml::Response::new()
            .say(voice_content.as_str())
            .build()
            .unwrap();

    Ok(welcome_intent)
}

async fn response_from_be_request_queue(config: RabbitIncomingMessage) -> Result<String, lapin::Error> {
    let confirmation = send_to_rabbit(config.clone()).await?;
    println!("{}", confirmation);
    let response = wait_for_response(config).await?;
    Ok(response.content)
}

/*
 * Publish the message to the Rabbit incoming_calls queue
 */
async fn send_to_rabbit(config: RabbitIncomingMessage) -> Result<String, lapin::Error> {
    let connection = lapin::Connection::connect(
        RABBITMQ_ADDR,
        lapin::ConnectionProperties::default().with_default_executor(8)
    ).await?;

    let channel = connection.create_channel().await?;

    let payload = match serde_json::to_string(&config) {
        Ok(payload) => payload,
        Err(_) => String::new()
    }.as_bytes().to_vec();

    let confirmation = channel.basic_publish(
        "",
        "incoming_calls",
        lapin::options::BasicPublishOptions::default(),
        payload,
        lapin::BasicProperties::default(), // setup confirmation correctly
    ).await?.await?;

    let message = match confirmation {
        lapin::publisher_confirm::Confirmation::NotRequested => "Outgoing message in queue".into(),
        _ => format!("Received confirmation - {:?}", confirmation)
    };

    connection.close(0, "complete").await?;

    Ok(message.into())
}

/*
 * Read from Rabbit Call SID request queue
 */
async fn wait_for_response(config: RabbitIncomingMessage) -> Result<RabbitResponseMessage, lapin::Error> {
    println!("awaiting response");
    let queue_name = config.call_sid.clone();
    let connection = lapin::Connection::connect(
        RABBITMQ_ADDR,
        lapin::ConnectionProperties::default().with_default_executor(8)
    ).await?;

    let channel = connection.create_channel().await?;
    let mut rabbit_response: RabbitResponseMessage = RabbitResponseMessage { content: "".to_string() };

    'ensure_queue:
    loop {

        let consumer = channel.basic_consume(
            &queue_name,
            "AIRLINE",
            lapin::options::BasicConsumeOptions::default(),
            lapin::types::FieldTable::default(),
        );

        let consumer = match consumer.await {
            Err(_) => continue 'ensure_queue,
            Ok(consumer) => consumer
        };

        let mut consumer_looper = consumer.into_iter();

        // only read one message from the queue
        'read_rabbit:
        while let Some(message) = consumer_looper.next() {
            let (_, delivery) = message.expect("Could not consume");
            delivery.ack(lapin::options::BasicAckOptions::default()).await.expect("ack");

            rabbit_response = match std::str::from_utf8(&delivery.data) {
                Ok(message) => RabbitResponseMessage { content: message.into() },
                Err(_) => RabbitResponseMessage { content: "No good message found.".into() }
            };
            break 'read_rabbit
        }
        break 'ensure_queue
    }

    connection.close(0, "complete").await?;

    Ok(rabbit_response)
}


/* ******* *
 * Helpers *
 * ******* */

/*
 * Ensure that the queue for incoming messages exists
 */
async fn setup_rabbit_queue() -> Result<(), lapin::Error> {
    let connection = lapin::Connection::connect(
        RABBITMQ_ADDR,
      lapin::ConnectionProperties::default().with_default_executor(8)
    ).await?;

    let channel = connection.create_channel().await?;

    channel.queue_declare(
        "incoming_calls",
        lapin::options::QueueDeclareOptions::default(),
        lapin::types::FieldTable::default()
    ).await?;

    connection.close(0, "complete").await?;

    Ok(())
}

/*
 * Setup logging - can be extended considerably
 */
fn enable_logs() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "tracing=info,warp=debug".to_owned());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE)
        .init();
}


