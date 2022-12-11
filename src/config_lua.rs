use anyhow::{bail, Result};
use mlua::{Lua, Value};

type V = serde_yaml::Value;

pub fn load_lua_config(_path: &str, src: &str) -> Result<V> {
  let lua = mlua::Lua::new();
  let v: Value = lua.load(src).eval().unwrap();
  conv_value(&lua, v)
}

fn conv_value(_lua: &Lua, value: Value) -> Result<V> {
  let v = match value {
    Value::Nil => V::Null,
    Value::Boolean(x) => V::Bool(x),
    Value::LightUserData(_) => todo!(),
    Value::Integer(x) => V::Number(x.into()),
    Value::Number(x) => V::Number(x.into()),
    Value::String(x) => V::String(x.to_string_lossy().to_string()),
    Value::Table(x) => {
      let mut map = serde_yaml::Mapping::new();
      for entry in x.pairs::<Value, Value>() {
        let (k, v) = entry.unwrap();
        map.insert(conv_value(_lua, k)?, conv_value(_lua, v)?);
      }
      V::Mapping(map)
    }
    Value::Function(_x) => todo!(),
    Value::Thread(_x) => todo!(),
    Value::UserData(_) => todo!(),
    Value::Error(err) => bail!(err),
  };
  Ok(v)
}
