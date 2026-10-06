//! Orders with lines. Two kinds of rule, kept apart on purpose:
//!
//! - Whose order it is: the row-level policies in toolsite.toml. Every read
//!   and write here for a person goes through `my_orders` and
//!   `my_order_lines` with `db::query_scoped`, so the handler cannot reach
//!   another person's order even by mistake.
//! - What may happen to it: the status rules in this file. Lines change only
//!   on a draft, a draft needs a line to be submitted, and only an approver
//!   decides.
//!
//! Totals are computed here, in SQL, from the prices stored on the lines.
//! A client never sends a price or a total.

wit_bindgen::generate!({
    path: "wit",
    world: "app",
});

use serde_json::{json, Map, Value};
use toolsite::app::{db, identity};

struct Handler;

impl Guest for Handler {
    fn handle(req: Request) -> Response {
        let body = || serde_json::from_slice::<Value>(&req.body).unwrap_or(Value::Null);
        let parts: Vec<&str> = req.path.trim_matches('/').split('/').collect();
        let result = match (req.method.as_str(), parts.as_slice()) {
            ("GET", ["api", "products"]) => products(),
            ("GET", ["api", "me"]) => me(),
            ("GET", ["api", "orders"]) => my_orders(&Value::Null),
            ("POST", ["api", "orders"]) => create_order(&body()),
            ("GET", ["api", "orders", id]) => id_of(id).and_then(order),
            ("POST", ["api", "orders", id, "lines"]) => {
                let args = body();
                id_of(id).and_then(|id| add_line(id, &args["sku"], &args["quantity"]))
            }
            ("DELETE", ["api", "orders", id, "lines", line]) => {
                id_of(id).and_then(|id| id_of(line).and_then(|line| remove_line(id, line)))
            }
            ("POST", ["api", "orders", id, "submit"]) => id_of(id).and_then(submit),
            ("GET", ["api", "queue"]) => queue(),
            ("POST", ["api", "orders", id, "decide"]) => {
                let args = body();
                id_of(id).and_then(|id| decide(id, args["decision"].as_str().unwrap_or("")))
            }

            // App tools. The platform signs the person in and calls these as
            // them; the arguments arrive under "arguments".
            ("POST", ["api", "tools", tool]) => {
                let args = body()["arguments"].clone();
                match *tool {
                    "create_order" => create_order(&args),
                    "add_line" => match args["order_id"].as_i64() {
                        Some(id) => add_line(id, &args["sku"], &args["quantity"]),
                        None => Err(fail(400, "Give order_id as a number.")),
                    },
                    "submit_order" => match args["order_id"].as_i64() {
                        Some(id) => submit(id),
                        None => Err(fail(400, "Give order_id as a number.")),
                    },
                    "my_orders" => my_orders(&args["status"]),
                    _ => Err(fail(404, "No such tool.")),
                }
            }
            _ => Err(fail(404, "No such route.")),
        };
        match result {
            Ok(value) => respond(200, &value),
            Err((status, value)) => respond(status, &value),
        }
    }
}

export!(Handler);

type Answer = Result<Value, (u16, Value)>;

fn fail(status: u16, message: &str) -> (u16, Value) {
    (status, json!({ "error": message }))
}

fn id_of(raw: &str) -> Result<i64, (u16, Value)> {
    raw.parse().map_err(|_| fail(400, "An id must be a number."))
}

// --- reads -------------------------------------------------------------------

fn products() -> Answer {
    rows(db::query("select sku, name, unit_price_cents from products order by name", &[]))
}

fn me() -> Answer {
    let user = identity::current_user().map(|u| json!({ "id": u.id, "email": u.email }));
    Ok(json!({ "user": user, "role": identity::current_role(), "is_approver": is_approver() }))
}

/// The person's orders with a line count and total each.
fn my_orders(status: &Value) -> Answer {
    let status = status.as_str().map(|s| db::Value::Text(s.into())).unwrap_or(db::Value::Null);
    let orders = rows(db::query_scoped(
        "select o.id, o.customer, o.status, o.created_at, o.submitted_at, o.decided_at, \
                count(l.id) as lines, coalesce(sum(l.quantity * l.unit_price_cents), 0) as total_cents \
         from my_orders o left join my_order_lines l on l.order_id = o.id \
         where (?1 is null or o.status = ?1) \
         group by o.id order by o.id desc limit 200",
        &[status],
    ))?;
    Ok(json!({ "orders": orders }))
}

/// One order with its lines and total. Only the person's own: the view
/// holds nothing else, so another id answers "not found".
fn order(id: i64) -> Answer {
    let found = rows(db::query_scoped(
        "select id, customer, status, created_at, submitted_at, decided_at, decided_by from my_orders where id = ?",
        &[db::Value::Integer(id)],
    ))?;
    let Some(mut order) = found.as_array().and_then(|a| a.first().cloned()) else {
        return Err(fail(404, &format!("You have no order {id}.")));
    };
    let lines = rows(db::query_scoped(
        "select l.id, l.sku, p.name, l.quantity, l.unit_price_cents, l.quantity * l.unit_price_cents as line_cents \
         from my_order_lines l join catalog p on p.sku = l.sku where l.order_id = ? order by l.id",
        &[db::Value::Integer(id)],
    ))?;
    let total: i64 = lines.as_array().into_iter().flatten().filter_map(|l| l["line_cents"].as_i64()).sum();
    order["lines"] = lines;
    order["total_cents"] = json!(total);
    Ok(json!({ "order": order }))
}

fn status_of(id: i64) -> Result<String, (u16, Value)> {
    let found = rows(db::query_scoped("select status from my_orders where id = ?", &[db::Value::Integer(id)]))?;
    found
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r["status"].as_str().map(str::to_string))
        .ok_or_else(|| fail(404, &format!("You have no order {id}.")))
}

// --- writes, with the status rules ---------------------------------------------

fn create_order(args: &Value) -> Answer {
    if identity::current_user().is_none() {
        return Err(fail(401, "Sign in to place an order."));
    }
    let customer = args["customer"].as_str().unwrap_or("").trim();
    if customer.is_empty() {
        return Err(fail(400, "Give the customer's name."));
    }
    // owner_id is filled by the policy on the way through the view.
    scoped("insert into my_orders (customer) values (?)", &[db::Value::Text(customer.into())])?;
    let created = rows(db::query_scoped(
        "select id from my_orders where owner_id = current_user() order by id desc limit 1",
        &[],
    ))?;
    let id = created
        .as_array()
        .and_then(|a| a.first())
        .and_then(|r| r["id"].as_i64())
        .ok_or_else(|| fail(500, "The order was not created."))?;
    for line in args["lines"].as_array().into_iter().flatten() {
        add_line(id, &line["sku"], &line["quantity"])?;
    }
    order(id)
}

fn add_line(id: i64, sku: &Value, quantity: &Value) -> Answer {
    let status = status_of(id)?;
    if status != "draft" {
        return Err(fail(409, &format!("Order {id} is {status}. Only a draft takes new lines.")));
    }
    let sku = sku.as_str().unwrap_or("").trim().to_uppercase();
    let quantity = quantity.as_i64().unwrap_or(0);
    if quantity < 1 {
        return Err(fail(400, "The quantity must be 1 or more."));
    }
    let price = rows(db::query("select unit_price_cents from products where sku = ?", &[db::Value::Text(sku.clone())]))?;
    let Some(price) = price.as_array().and_then(|a| a.first()).and_then(|r| r["unit_price_cents"].as_i64()) else {
        return Err(fail(400, &format!("There is no product '{sku}'.")));
    };
    // The same product twice becomes one line with the quantities added.
    let merged = db::query_scoped(
        "update my_order_lines set quantity = quantity + ? where order_id = ? and sku = ?",
        &[db::Value::Integer(quantity), db::Value::Integer(id), db::Value::Text(sku.clone())],
    )
    .map_err(|e| db_fail(&e))?;
    if merged.rows_affected == 0 {
        scoped(
            "insert into my_order_lines (order_id, sku, quantity, unit_price_cents) values (?, ?, ?, ?)",
            &[db::Value::Integer(id), db::Value::Text(sku), db::Value::Integer(quantity), db::Value::Integer(price)],
        )?;
    }
    order(id)
}

fn remove_line(id: i64, line: i64) -> Answer {
    let status = status_of(id)?;
    if status != "draft" {
        return Err(fail(409, &format!("Order {id} is {status}. Only a draft can change.")));
    }
    scoped(
        "delete from my_order_lines where id = ? and order_id = ?",
        &[db::Value::Integer(line), db::Value::Integer(id)],
    )?;
    order(id)
}

fn submit(id: i64) -> Answer {
    let status = status_of(id)?;
    if status == "submitted" {
        // Asking twice is not an error: the order is where it was asked to be.
        return order(id);
    }
    if status != "draft" {
        return Err(fail(409, &format!("Order {id} is {status} and cannot be submitted again.")));
    }
    let lines = rows(db::query_scoped("select count(*) as n from my_order_lines where order_id = ?", &[db::Value::Integer(id)]))?;
    if lines.as_array().and_then(|a| a.first()).and_then(|r| r["n"].as_i64()).unwrap_or(0) == 0 {
        return Err(fail(409, &format!("Order {id} has no lines. Add one before you submit it.")));
    }
    scoped(
        "update my_orders set status = 'submitted', submitted_at = cast(strftime('%s','now') as integer) where id = ?",
        &[db::Value::Integer(id)],
    )?;
    order(id)
}

// --- approvers ---------------------------------------------------------------

fn is_approver() -> bool {
    identity::current_role().as_deref() == Some("approver")
}

/// Every submitted order, whoever placed it. This reads past the policy with
/// `db::query`, which is why it checks the role first.
fn queue() -> Answer {
    if !is_approver() {
        return Err(fail(403, "Only an approver sees the queue."));
    }
    let orders = rows(db::query(
        "select o.id, o.customer, o.submitted_at, count(l.id) as lines, \
                coalesce(sum(l.quantity * l.unit_price_cents), 0) as total_cents \
         from orders o left join order_lines l on l.order_id = o.id \
         where o.status = 'submitted' group by o.id order by o.submitted_at",
        &[],
    ))?;
    Ok(json!({ "orders": orders }))
}

fn decide(id: i64, decision: &str) -> Answer {
    if !is_approver() {
        return Err(fail(403, "Only an approver decides orders."));
    }
    let status = match decision {
        "approve" => "approved",
        "reject" => "rejected",
        _ => return Err(fail(400, "Decide with approve or reject.")),
    };
    let who = identity::current_user().map(|u| u.email).unwrap_or_default();
    let changed = db::query(
        "update orders set status = ?, decided_at = cast(strftime('%s','now') as integer), decided_by = ? \
         where id = ? and status = 'submitted'",
        &[db::Value::Text(status.into()), db::Value::Text(who), db::Value::Integer(id)],
    )
    .map_err(|e| db_fail(&e))?;
    if changed.rows_affected == 0 {
        return Err(fail(409, &format!("Order {id} is not waiting for a decision.")));
    }
    Ok(json!({ "id": id, "status": status }))
}

// --- helpers -------------------------------------------------------------------

fn scoped(sql: &str, params: &[db::Value]) -> Result<db::Rows, (u16, Value)> {
    db::query_scoped(sql, params).map_err(|e| db_fail(&e))
}

fn db_fail(e: &db::Error) -> (u16, Value) {
    match e {
        db::Error::Failed(m) => fail(400, m),
        db::Error::Denied(m) => fail(403, m),
    }
}

fn value_json(v: &db::Value) -> Value {
    match v {
        db::Value::Null => Value::Null,
        db::Value::Integer(i) => json!(i),
        db::Value::Real(f) => json!(f),
        db::Value::Text(s) => json!(s),
    }
}

fn rows(result: Result<db::Rows, db::Error>) -> Answer {
    let rows = result.map_err(|e| db_fail(&e))?;
    Ok(Value::Array(
        rows.values
            .iter()
            .map(|row| Value::Object(rows.columns.iter().cloned().zip(row.iter().map(value_json)).collect::<Map<_, _>>()))
            .collect(),
    ))
}

fn respond(status: u16, value: &Value) -> Response {
    Response {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: value.to_string().into_bytes(),
    }
}
