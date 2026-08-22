//! 临时工具：重置 llm_providers / llm_models 表
use tokio_postgres::NoTls;
use urlencoding::encode;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn_str = format!(
        "postgres://{}:{}@{}:{}/{}?connect_timeout=5",
        encode("master"),
        encode(""),
        "localhost",
        "5432",
        "marvelnet"
    );
    let (client, connection) = tokio_postgres::connect(&conn_str, NoTls).await?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            eprintln!("[reset] {e}");
        }
    });
    client
        .batch_execute("DROP TABLE IF EXISTS llm_models CASCADE")
        .await?;
    client
        .batch_execute("DROP TABLE IF EXISTS llm_providers CASCADE")
        .await?;
    println!("[reset] 完成");
    Ok(())
}
