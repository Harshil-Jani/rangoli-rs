//! Validating submitted data against a model: shared by the admin and `ModelForm`.

use crate::admin::Field;
use crate::auth;
use crate::orm::{FieldType, Model, ModelMeta, Value};
use crate::DateTime;
use std::collections::HashMap;
use std::marker::PhantomData;

/// Validate a submitted form against the model; password fields come back hashed.
pub(crate) async fn validate(
    meta: &'static ModelMeta,
    form: &HashMap<String, String>,
    is_add: bool,
    readonly: &[&str],
) -> Result<Vec<(&'static str, Value, FieldType)>, HashMap<String, String>> {
    let (mut cols, mut errors) = (vec![], HashMap::new());
    for f in meta.fields {
        if f.is_auto() {
            if f.auto_now || is_add {
                cols.push((f.name, f.auto_value(), f.ty));
            }
            continue;
        }
        // Columns Rangoli only shows as text are never written; the database keeps its value.
        if matches!(f.ty, FieldType::Opaque | FieldType::Vector(_)) {
            continue;
        }
        // Read-only fields ignore whatever was posted; new rows get NULL or the type's zero value.
        if readonly.contains(&f.name) {
            if is_add {
                cols.push((f.name, if f.null { Value::Null } else { crate::migrate::zero(f.ty) }, f.ty));
            }
            continue;
        }
        let raw = form.get(f.name).map(|s| s.trim()).unwrap_or("");
        let v = if f.ty == FieldType::Bool {
            Ok(Value::Bool(!raw.is_empty()))
        } else if raw.is_empty() {
            if f.password && !is_add {
                continue; // keep the existing hash
            }
            if f.null {
                Ok(Value::Null)
            } else {
                Err("This field is required.".to_string())
            }
        } else {
            match f.ty {
                FieldType::Int => raw.parse().map(Value::Int).map_err(|_| "Enter a whole number.".to_string()),
                FieldType::DateTime => DateTime::parse(raw).map(Value::from).ok_or("Enter a valid date and time.".to_string()),
                FieldType::Date => crate::Date::parse(raw).map(Value::from).ok_or("Enter a valid date.".to_string()),
                FieldType::Decimal(p) => {
                    crate::decimal::parse_units(raw, p).map(Value::Int).ok_or(format!("Enter a number with at most {p} decimal places."))
                }
                FieldType::Uuid => crate::Uuid::parse(raw).map(Value::from).ok_or("Enter a valid UUID.".to_string()),
                FieldType::Json => serde_json::from_str::<serde_json::Value>(raw)
                    .map(|j| Value::Text(j.to_string()))
                    .map_err(|e| format!("Enter valid JSON ({e}).")),
                FieldType::Varchar(_) if f.choices.is_some_and(|c| !c.iter().any(|(v, _)| *v == raw)) => {
                    Err(format!("Select a valid choice. {raw} is not one of the available choices."))
                }
                FieldType::Float => {
                    raw.parse::<f64>().ok().filter(|x| x.is_finite()).map(Value::Float).ok_or("Enter a number.".to_string())
                }
                // Postgres rejects NUL in text; refuse it everywhere, like Django's validator.
                FieldType::Varchar(_) | FieldType::Text if raw.contains('\0') => Err("Null characters are not allowed.".to_string()),
                FieldType::Varchar(n) if raw.chars().count() > n as usize => {
                    Err(format!("Ensure this value has at most {n} characters (it has {}).", raw.chars().count()))
                }
                FieldType::Text | FieldType::Varchar(_) if f.password => {
                    auth::hash_password(raw).await.map(Value::Text).map_err(|e| e.to_string())
                }
                _ => Ok(Value::Text(raw.to_string())),
            }
        };
        match v {
            Ok(v) => cols.push((f.name, v, f.ty)),
            Err(e) => {
                errors.insert(f.name.to_string(), e);
            }
        }
    }
    if errors.is_empty() {
        Ok(cols)
    } else {
        Err(errors)
    }
}

/// Field name -> message, ready to show next to each input.
pub type FormErrors = HashMap<String, String>;

/// Django's `ModelForm`: validate submitted form data with the same rules as the admin
/// and get a typed model back.
///
/// ```ignore
/// async fn signup(Form(data): Form<Vec<(String, String)>>) -> rangoli::Result<Response> {
///     match ModelForm::<Author>::new().fields([&Author::NAME, &Author::EMAIL]).validate(&data).await {
///         Ok(mut author) => { author.save().await?; Ok(Redirect::to("/thanks").into_response()) }
///         Err(errors) => Ok(render("signup.html", context! { errors, data })?.into_response()),
///     }
/// }
/// ```
pub struct ModelForm<M> {
    only: Option<Vec<&'static str>>,
    instance: Option<M>,
    _m: PhantomData<fn() -> M>,
}

impl<M: Model> Default for ModelForm<M> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: Model> ModelForm<M> {
    pub fn new() -> Self {
        ModelForm { only: None, instance: None, _m: PhantomData }
    }

    /// Accept only these fields from the form; the rest keep their current or default values.
    pub fn fields<const N: usize>(mut self, cols: [&dyn Field<M>; N]) -> Self {
        self.only = Some(cols.iter().map(|c| c.name()).collect());
        self
    }

    /// Edit an existing object instead of creating a new one.
    pub fn instance(mut self, m: M) -> Self {
        self.instance = Some(m);
        self
    }

    /// Validate `data` (form pairs, as from `Form<Vec<(String, String)>>`). Nothing is saved.
    pub async fn validate(&self, data: &[(String, String)]) -> std::result::Result<M, FormErrors> {
        let meta = M::meta();
        let form: HashMap<String, String> = data.iter().cloned().collect();
        let adding = self.instance.is_none();
        // Fields outside `fields(...)` are treated as read-only: whatever was posted is ignored.
        let readonly: Vec<&str> = match &self.only {
            Some(only) => meta.fields.iter().map(|f| f.name).filter(|n| !only.contains(n)).collect(),
            None => vec![],
        };
        let cols = validate(meta, &form, adding, &readonly).await?;
        let mut values = match &self.instance {
            Some(m) => m.values(),
            None => meta
                .fields
                .iter()
                .map(|f| match f.default {
                    Some(d) => d.value(),
                    None if f.null => Value::Null,
                    None => crate::migrate::zero(f.ty),
                })
                .collect(),
        };
        for (name, v, _) in cols.into_iter().filter(|(n, ..)| !readonly.contains(n)) {
            if let Some(i) = meta.fields.iter().position(|f| f.name == name) {
                values[i] = v;
            }
        }
        let id = self.instance.as_ref().and_then(Model::pk);
        // The model's whole-row rules (App::validate) too.
        let all: Vec<(&'static str, Value, FieldType)> = meta.fields.iter().zip(&values).map(|(f, v)| (f.name, v.clone(), f.ty)).collect();
        match crate::validators::check(meta, id, &all).await {
            Ok(Ok(())) => {}
            Ok(Err(errors)) => return Err(errors),
            Err(e) => return Err([("__all__".to_string(), e.to_string())].into()),
        }
        M::from_values(id, values).map_err(|e| [("__all__".to_string(), e.to_string())].into())
    }

    /// The form's fields as HTML (labels, inputs typed by field, help from doc comments, and
    /// `errors` next to their fields), for `{{ form|safe }}` inside your own `<form>`. `data` is
    /// what was posted, so a refused form keeps what was typed; without it, the instance's
    /// values (or defaults) are shown.
    pub fn html(&self, data: &[(String, String)], errors: &FormErrors) -> String {
        let meta = M::meta();
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
        let posted: HashMap<&str, &str> = data.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let stored: Vec<Value> = self.instance.as_ref().map(Model::values).unwrap_or_default();
        let mut out = String::new();
        if let Some(e) = errors.get("__all__") {
            out.push_str(&format!("<p class=\"form-error\" role=\"alert\">{}</p>\n", esc(e)));
        }
        for (i, f) in meta.fields.iter().enumerate() {
            let shown = self.only.as_ref().is_none_or(|o| o.contains(&f.name));
            if !shown || f.is_auto() || f.private || matches!(f.ty, FieldType::Opaque | FieldType::Vector(_)) {
                continue;
            }
            let value = match posted.get(f.name) {
                Some(v) => v.to_string(),
                None if f.password => String::new(),
                None => match stored.get(i).or(f.default.map(|d| d.value()).as_ref()) {
                    Some(Value::Text(s)) => s.clone(),
                    Some(Value::Int(n)) if f.ty == FieldType::DateTime => DateTime::from_unix(*n).input_value(),
                    Some(Value::Int(n)) if f.ty == FieldType::Date => crate::Date::from_days(*n).to_string(),
                    Some(Value::Int(n)) => match f.ty {
                        FieldType::Decimal(p) => crate::decimal::format_units(*n, p),
                        _ => n.to_string(),
                    },
                    Some(Value::Float(x)) => x.to_string(),
                    Some(Value::Bool(b)) => (if *b { "on" } else { "" }).into(),
                    _ => String::new(),
                },
            };
            let id = format!("id_{}", f.name);
            let label = f.name.replace('_', " ");
            let label = label[..1].to_uppercase() + &label[1..];
            let required = if !f.null && f.ty != FieldType::Bool && !f.password { " required" } else { "" };
            let control = if let Some(choices) = f.choices {
                let opts: String = choices
                    .iter()
                    .map(|(v, l)| format!("<option value=\"{}\"{}>{}</option>", esc(v), if *v == value { " selected" } else { "" }, esc(l)))
                    .collect();
                format!("<select name=\"{}\" id=\"{id}\"{required}>{opts}</select>", f.name)
            } else {
                match f.ty {
                    FieldType::Bool => format!(
                        "<input type=\"checkbox\" name=\"{}\" id=\"{id}\"{}>",
                        f.name,
                        if value.is_empty() { "" } else { " checked" }
                    ),
                    FieldType::Text | FieldType::Json => {
                        format!("<textarea name=\"{}\" id=\"{id}\" rows=\"6\"{required}>{}</textarea>", f.name, esc(&value))
                    }
                    ty => {
                        let kind = match (f.password, ty) {
                            (true, _) => "password",
                            (_, FieldType::Int | FieldType::Float) => "number",
                            (_, FieldType::Date) => "date",
                            (_, FieldType::DateTime) => "datetime-local",
                            _ => "text",
                        };
                        let max = match ty {
                            FieldType::Varchar(n) => format!(" maxlength=\"{n}\""),
                            _ => String::new(),
                        };
                        let step = if ty == FieldType::Float { " step=\"any\"" } else { "" };
                        format!("<input type=\"{kind}\" name=\"{}\" id=\"{id}\" value=\"{}\"{max}{step}{required}>", f.name, esc(&value))
                    }
                }
            };
            let help = f.help.map(|h| format!("<small id=\"{id}_help\">{}</small>", esc(h))).unwrap_or_default();
            let error =
                errors.get(f.name).map(|e| format!("<span class=\"field-error\" id=\"{id}_error\">{}</span>", esc(e))).unwrap_or_default();
            out.push_str(&format!("<p><label for=\"{id}\">{}</label> {control} {help}{error}</p>\n", esc(&label)));
        }
        out
    }
}
