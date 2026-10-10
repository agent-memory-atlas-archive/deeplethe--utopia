//! Exercise expression writes and conversions through authentication and the real materializer.
use crate::state::AppState;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

#[tokio::test]
async fn expression_operands_round_trip_and_execute_through_authenticated_routes(
) -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = sqlx::PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;
    let (org, ws, kb, user, class, input, output) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let dir = tempfile::tempdir()?;
    let cfg = utopia_core::config::AppConfig {
        data_dir: dir.path().to_string_lossy().into_owned(),
        ..Default::default()
    };
    let state = AppState::new(
        pool.clone(),
        &cfg,
        Arc::new(utopia_search::SearchIndex::open(
            &dir.path().join("search"),
        )?),
        "test-only".into(),
    );
    let result = async {
        sqlx::raw_sql(&format!("INSERT INTO organizations(id,name) VALUES('{org}','metadata-test');
        INSERT INTO workspaces(id,org_id,name) VALUES('{ws}','{org}','metadata-test');
        INSERT INTO knowledge_bases(id,workspace_id,name) VALUES('{kb}','{ws}','metadata-test');
        INSERT INTO users(id,org_id,email,password_hash,display_name) VALUES('{user}','{org}','{user}@example.test','unused','Test');
        INSERT INTO kb_members(kb_id,user_id,role) VALUES('{kb}','{user}','editor');
        INSERT INTO entity_types(id,kb_id,key,label) VALUES('{class}','{kb}','thing','Thing');
        INSERT INTO relation_types(id,kb_id,key,label,kind,datatype) VALUES('{input}','{kb}','input','Input','attribute','number'),('{output}','{kb}','output','Output','attribute','number');"))
            .execute(&pool).await?;
        let token = crate::auth::issue_token(&state, user)?;
        let call = |method: &'static str, path: String, body: Value| {
            let state = state.clone(); let token = token.clone();
            async move {
                let request = Request::builder().method(method).uri(path).header("authorization", format!("Bearer {token}"))
                    .header("content-type","application/json").body(Body::from(body.to_string()))?;
                let response = super::router(state, &Default::default()).oneshot(request).await?;
                let status = response.status();
                let body = axum::body::to_bytes(response.into_body(), 65536).await?;
                anyhow::Ok((status, serde_json::from_slice::<Value>(&body)?))
            }
        };
        let base = format!("/api/v1/kbs/{kb}/rules");
        let expr = json!({"op":"sub","l":{"attr":input},"r":{"attr":output}});
        let definition = json!({"name":"Threshold", "subject_type_id":class, "conclusion":"attribute", "conclude_predicate_id":output,"conclude_value":"passed","conditions":[{"predicate_id":input,"op":"gt","operand":expr,"group":2}]});
        let (status, created) = call("POST",base.clone(),definition.clone()).await?;
        anyhow::ensure!(status.is_success(), "expression POST: {status} {created}");
        let id = created["id"].as_str().unwrap();
        let path = format!("{base}/{id}");
        let entity = Uuid::now_v7();
        sqlx::query("INSERT INTO entities(id,kb_id,type_id,canonical_name) VALUES($1,$2,$3,'Subject')")
            .bind(entity).bind(kb).bind(class).execute(&pool).await?;
        let mut facts = Vec::new();
        for (predicate, value) in [(input,100), (output,40)] {
            let fact = Uuid::now_v7();
            sqlx::query("INSERT INTO facts(id,kb_id,subject_id,predicate_id,object_value,valid_from,valid_from_precision,confidence) VALUES($1,$2,$3,$4,$5,'2023-01-01','day',0.9)")
                .bind(fact).bind(kb).bind(entity).bind(predicate).bind(json!({"value":value})).execute(&pool).await?;
            facts.push(fact);
        }
        // A non-computed conclusion ensures this exercises the condition operand.
        for (op, hits) in [("gt",1),("gte",1),("lt",0),("lte",0)] {
            let conditions = json!([{"predicate_id":input,"op":op,"operand":expr,"group":2}]);
            anyhow::ensure!(call("PATCH",path.clone(),json!({"conditions":conditions})).await?.0.is_success());
            let (_, read) = call("GET",base.clone(),json!(null)).await?;
            anyhow::ensure!(read["rules"][0]["conditions"][0]["operand"] == expr);
            let report = utopia_store::reasoning::materialize(&pool,kb).await?;
            anyhow::ensure!(report.rule_hits == hits, "{op}: {report:?}");
            if hits == 1 {
                let premises: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT fd.premise_fact_id FROM fact_derivations fd JOIN derived_facts d ON d.id=fd.derived_fact_id WHERE d.kb_id=$1 AND d.invalidated_at IS NULL AND fd.premise_fact_id IS NOT NULL")
                    .bind(kb).fetch_all(&pool).await?;
                anyhow::ensure!(facts.iter().all(|f| premises.contains(f)));
            }
        }
        let nested = json!({"op":"div","l":{"attr":input},"r":{"op":"div","l":{"attr":output},"r":{"const":"2"}}});
        let conditions = json!([{"predicate_id":input,"op":"gt","operand":nested,"group":2},{"predicate_id":output,"op":"present","group":7}]);
        anyhow::ensure!(call("PATCH",path.clone(),json!({"conditions":conditions})).await?.0.is_success());
        let (_, before) = call("GET",base.clone(),json!(null)).await?;
        anyhow::ensure!(before["rules"][0]["conditions"][0]["operand"] == nested);
        anyhow::ensure!(before["rules"][0]["conditions"][1]["group"] == 7);
        // A bad second condition must not commit the name or first condition.
        for bad in [json!({"attr":Uuid::now_v7()}),json!({"op":"pow","l":{"attr":input},"r":{"const":2}}),json!({"const":"NaN"}),json!({"op":"add","l":{"attr":input}})] {
            let patch = json!({"name":"Must not commit", "conditions":[{"predicate_id":input,"op":"gt","operand":5,"group":2},{"predicate_id":input,"op":"gt","operand":bad,"group":7}]});
            anyhow::ensure!(call("PATCH",path.clone(),patch).await?.0 == StatusCode::UNPROCESSABLE_ENTITY);
            anyhow::ensure!(call("GET",base.clone(),json!(null)).await?.1 == before);
        }
        let foreign_kb=Uuid::now_v7(); let foreign=Uuid::now_v7(); let relation=Uuid::now_v7();
        sqlx::query("INSERT INTO knowledge_bases(id,workspace_id,name) VALUES($1,$2,'foreign')").bind(foreign_kb).bind(ws).execute(&pool).await?;
        sqlx::query("INSERT INTO relation_types(id,kb_id,key,label,kind) VALUES($1,$2,'foreign','Foreign','attribute'),($3,$4,'edge','Edge','relation')")
            .bind(foreign).bind(foreign_kb).bind(relation).bind(kb).execute(&pool).await?;
        for (reference,code) in [(foreign,"unknown_predicate"),(relation,"not_an_attribute")] {
            let mut request=definition.clone(); request["name"]=json!("Rejected");
            request["conditions"][0]["operand"]=json!({"attr":reference});
            let (status,error)=call("POST",base.clone(),request).await?;
            anyhow::ensure!(status==StatusCode::UNPROCESSABLE_ENTITY && error["code"]==code);
            anyhow::ensure!(call("GET",base.clone(),json!(null)).await?.1==before);
        }
        // Existing scalar, set, range, presence contracts stay separate.
        for (op, operand) in [("gt",json!(5)),("gte",json!("5")),("lt",json!(500)),("lte",json!("500")),("between",json!([0,100])),("in",json!([100])),("not_in",json!([0])),("present",Value::Null)] {
            let c = json!([{"predicate_id":input,"op":op,"operand":operand}]);
            anyhow::ensure!(call("PATCH",path.clone(),json!({"conditions":c})).await?.0.is_success(), "scalar {op}");
            if ["between","in","not_in","present"].contains(&op) {
                let c=json!([{"predicate_id":input,"op":op,"operand":expr}]);
                anyhow::ensure!(call("PATCH",path.clone(),json!({"conditions":c})).await?.0 == StatusCode::UNPROCESSABLE_ENTITY);
            }
        }
        let mut deep=json!({"attr":input});
        for _ in 0..4 { deep=json!({"op":"sub","l":deep,"r":{"const":1}}); }
        let c=json!([{"predicate_id":input,"op":"gt","operand":deep}]);
        anyhow::ensure!(call("PATCH",path.clone(),json!({"conditions":c})).await?.0.is_success());
        anyhow::ensure!(utopia_store::reasoning::materialize(&pool,kb).await?.rule_hits == 1);
        deep=json!({"op":"sub","l":deep,"r":{"const":1}});
        let c=json!([{"predicate_id":input,"op":"gt","operand":deep}]);
        let (status,error)=call("PATCH",path.clone(),json!({"conditions":c})).await?;
        anyhow::ensure!(status == StatusCode::UNPROCESSABLE_ENTITY && error["code"] == "expression_too_deep");

        // The same authenticated writer accepts conversion text, but neither
        // the live definition nor its history may retain executable text.
        let fields: std::collections::HashMap<_,_> = ["amount", "channel", "recorded_on", "paid_amount", "channel_name", "recorded_month", "eligible"]
            .into_iter().map(|key| (key,Uuid::now_v7())).collect();
        for (key, datatype) in [("amount","text"),("channel","number"),("recorded_on","date"),("paid_amount","number"),("channel_name","text"),("recorded_month","date"),("eligible","text")] {
            sqlx::query("INSERT INTO relation_types(id,kb_id,key,label,kind,datatype) VALUES($1,$2,$3,$3,'attribute',$4)")
                .bind(fields[key]).bind(kb).bind(key).bind(datatype).execute(&pool).await?;
        }
        let mut conversion_facts = std::collections::HashMap::new();
        for (key,value) in [("amount",json!("12550")),("channel",json!(1)),("recorded_on",json!("2024-03-01T00:30:00+08:00"))] {
            let fact = Uuid::now_v7();
            sqlx::query("INSERT INTO facts(id,kb_id,subject_id,predicate_id,object_value,valid_from,valid_from_precision,valid_to,valid_to_precision,confidence) VALUES($1,$2,$3,$4,$5,'2023-06-01','day','2024-06-01','day',0.9)")
                .bind(fact).bind(kb).bind(entity).bind(fields[key]).bind(json!({"value":value})).execute(&pool).await?;
            conversion_facts.insert(key,fact);
        }
        let mut conversion_rules = std::collections::HashMap::new();
        for (target,text) in [
            ("paid_amount","CAST(amount AS DOUBLE PRECISION) / 100"),
            ("channel_name","CASE channel WHEN 1 THEN 'app' WHEN 2 THEN 'web' ELSE 'other' END"),
            ("recorded_month","date_trunc('month', recorded_on)"),
        ] {
            let request = json!({"name":target,"subject_type_id":class,"conclusion":"computed","conclude_predicate_id":fields[target],
                "conclude_expr":{"expression":text},"conditions":[{"predicate_id":input,"op":"present"}]});
            let (status,body) = call("POST",base.clone(),request).await?;
            anyhow::ensure!(status.is_success(), "{target}: {status} {body}");
            conversion_rules.insert(target,body["id"].as_str().unwrap().parse::<Uuid>()?);
        }
        let downstream = json!({"name":"App customer","subject_type_id":class,"conclusion":"attribute","conclude_predicate_id":fields["eligible"],
            "conclude_value":"yes","conditions":[{"predicate_id":fields["channel_name"],"op":"in","operand":["app"]}]});
        anyhow::ensure!(call("POST",base.clone(),downstream).await?.0.is_success());
        let (_,read) = call("GET",base.clone(),json!(null)).await?;
        for id in conversion_rules.values() {
            let row = read["rules"].as_array().unwrap().iter().find(|r|r["id"]==id.to_string()).unwrap();
            anyhow::ensure!(row["conclude_expr"].is_object() && row["conclude_expr"].get("expression").is_none());
        }
        let targets = vec![fields["paid_amount"],fields["channel_name"],fields["recorded_month"],fields["eligible"]];
        utopia_store::reasoning::materialize(&pool,kb).await?;
        let results: Vec<(Uuid,Value,bool)> = sqlx::query_as("SELECT predicate_id,object_value->'value',valid_from='2023-06-01'::timestamptz AND valid_to='2024-06-01'::timestamptz FROM derived_facts WHERE kb_id=$1 AND subject_id=$2 AND predicate_id=ANY($3) AND invalidated_at IS NULL")
            .bind(kb).bind(entity).bind(&targets).fetch_all(&pool).await?;
        anyhow::ensure!(results.len()==4,"conversion results: {results:?}");
        for (target,expected) in [("paid_amount",json!(125.5)),("channel_name",json!("app")),("recorded_month",json!("2024-02-01")),("eligible",json!("yes"))] {
            let result = results.iter().find(|r|r.0==fields[target]).unwrap();
            anyhow::ensure!(result.1==expected && result.2,"{target}: {result:?}");
        }
        let premises: Vec<Uuid> = sqlx::query_scalar("SELECT fd.premise_fact_id FROM fact_derivations fd JOIN derived_facts d ON d.id=fd.derived_fact_id WHERE d.kb_id=$1 AND d.predicate_id=$2 AND d.invalidated_at IS NULL AND fd.premise_fact_id IS NOT NULL")
            .bind(kb).bind(fields["channel_name"]).fetch_all(&pool).await?;
        anyhow::ensure!(premises.len()==2 && premises.contains(&facts[0]) && premises.contains(&conversion_facts["channel"]));

        // A later definition retires the old label and its downstream conclusion.
        let channel_path = format!("{base}/{}",conversion_rules["channel_name"]);
        let patch = json!({"conclusion":"computed","conclude_predicate_id":fields["channel_name"],
            "conclude_expr":{"expression":"CASE channel WHEN 1 THEN 'mobile' END"}});
        anyhow::ensure!(call("PATCH",channel_path.clone(),patch).await?.0.is_success());
        utopia_store::reasoning::materialize(&pool,kb).await?;
        let labels: Vec<Value> = sqlx::query_scalar("SELECT object_value->'value' FROM derived_facts WHERE kb_id=$1 AND predicate_id=$2 AND invalidated_at IS NULL")
            .bind(kb).bind(fields["channel_name"]).fetch_all(&pool).await?;
        anyhow::ensure!(labels==vec![json!("mobile")]);
        let retired: i64 = sqlx::query_scalar("SELECT count(*) FROM derived_facts WHERE kb_id=$1 AND predicate_id=ANY($2) AND invalidated_at IS NOT NULL")
            .bind(kb).bind(vec![fields["channel_name"],fields["eligible"]]).fetch_one(&pool).await?;
        anyhow::ensure!(retired==2);
        let versions: Vec<Value> = sqlx::query_scalar("SELECT definition->'conclude_expr' FROM attribute_rule_versions WHERE rule_id=$1 ORDER BY seq")
            .bind(conversion_rules["channel_name"]).fetch_all(&pool).await?;
        anyhow::ensure!(versions.len()==2 && versions.iter().all(|v|v.get("expression").is_none()));
        anyhow::ensure!(versions[0]["when"][0]["then"]=="app" && versions[1]["when"][0]["then"]=="mobile");

        // Neither a missing name nor a foreign UUID can partly rename a rule.
        let (_,before) = call("GET",base.clone(),json!(null)).await?;
        for (bad,code) in [
            (json!({"expression":"missing_amount / 100"}),"unknown_expression_attribute"),
            (json!({"expression":"amount; DELETE FROM jobs"}),"bad_expression"),
            (json!({"expression":"amount / 100","future":true}),"bad_expression"),
            (json!({"case":{"attr":foreign},"when":[{"is":1,"then":"app"}]}),"unknown_predicate"),
            (json!({"date_trunc":"month","expr":{"attr":relation}}),"not_an_attribute"),
        ] {
            let patch=json!({"name":"Must not commit","conclusion":"computed","conclude_predicate_id":fields["channel_name"],"conclude_expr":bad});
            let (status,error)=call("PATCH",channel_path.clone(),patch).await?;
            anyhow::ensure!(status==StatusCode::UNPROCESSABLE_ENTITY && error["code"]==code,"rejected conversion: {status} {error}");
            anyhow::ensure!(call("GET",base.clone(),json!(null)).await?.1==before);
        }
        // Condition text uses the same compiler; scalar "5" above remains scalar.
        let conditions=json!([{"predicate_id":input,"op":"gt","operand":{"expression":"CAST(amount AS DOUBLE PRECISION) / 100"}}]);
        anyhow::ensure!(call("PATCH",path.clone(),json!({"conditions":conditions})).await?.0.is_success());
        let stored: Value = sqlx::query_scalar("SELECT operand FROM attribute_rule_conditions WHERE rule_id=$1")
            .bind(id.parse::<Uuid>()?).fetch_one(&pool).await?;
        anyhow::ensure!(stored["l"]["cast"]=="number" && stored.get("expression").is_none());

        // Invalid observations and an unmatched code remove the old results;
        // no zero, default date or stale downstream label survives recomputation.
        for (key,value) in [("amount",json!("not a number")),("channel",json!(9)),("recorded_on",json!("2024-02-30"))] {
            sqlx::query("UPDATE facts SET object_value=$2 WHERE id=$1").bind(conversion_facts[key])
                .bind(json!({"value":value})).execute(&pool).await?;
        }
        utopia_store::reasoning::materialize(&pool,kb).await?;
        let active: i64=sqlx::query_scalar("SELECT count(*) FROM derived_facts WHERE kb_id=$1 AND predicate_id=ANY($2) AND invalidated_at IS NULL")
            .bind(kb).bind(&targets).fetch_one(&pool).await?;
        anyhow::ensure!(active==0);
        let foreign_request=json!({"name":"Foreign","subject_type_id":class,"conclusion":"computed","conclude_predicate_id":fields["paid_amount"],
            "conclude_expr":{"expression":"CAST(amount AS DOUBLE PRECISION) / 100"},"conditions":[{"predicate_id":input,"op":"present"}]});
        // An inaccessible base stays indistinguishable from a missing one.
        anyhow::ensure!(call("POST",format!("/api/v1/kbs/{foreign_kb}/rules"),foreign_request.clone()).await?.0==StatusCode::NOT_FOUND);
        sqlx::query("UPDATE kb_members SET role='viewer' WHERE user_id=$1").bind(user).execute(&pool).await?;
        anyhow::ensure!(call("POST",base.clone(),foreign_request).await?.0==StatusCode::FORBIDDEN);
        anyhow::ensure!(call("POST",base.clone(),definition).await?.0 == StatusCode::FORBIDDEN);
        anyhow::Ok(())
    }.await;
    sqlx::query("DELETE FROM organizations WHERE id=$1")
        .bind(org)
        .execute(&pool)
        .await?;
    result
}
