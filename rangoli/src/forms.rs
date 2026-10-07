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
                cols.push((f.name, Value::Int(DateTime::now().unix()), f.ty));
            }
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
        M::from_values(id, values).map_err(|e| [("__all__".to_string(), e.to_string())].into())
    }
}
