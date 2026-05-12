#[tokio::main]
async fn main() {
    let ctx = datafusion::prelude::SessionContext::new();
    let res = ctx.sql("SELECT sqrt('hello')").await;
    println!("{:?}", res);
}
