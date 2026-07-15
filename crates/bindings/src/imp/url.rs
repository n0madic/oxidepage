//! `URL` implementation, backed by the `url` crate.
//!
//! The component getters/setters live in [`crate::imp::url_parts`], shared with
//! the `HTMLHyperlinkElementUtils` mixin.

use std::rc::Rc;

use oxidepage_js::{HostCall, JsThrow, JsValue};
use url::Url;

use crate::cx::BindCx;
use crate::imp::url_parts as parts;
use crate::netdata::{UrlData, UrlSearchParamsData};
use crate::state::HostData;

type UrlRef = Rc<UrlData>;

pub(crate) fn constructor(
    cx: &BindCx<'_>,
    _call: &HostCall,
    url: String,
    base: Option<String>,
) -> Result<JsValue, JsThrow> {
    let parsed = match base {
        Some(base) => {
            let base = Url::parse(&base).map_err(|_| {
                JsThrow::Type("Failed to construct 'URL': Invalid base URL".to_owned())
            })?;
            base.join(&url)
        }
        None => Url::parse(&url),
    }
    .map_err(|_| JsThrow::Type(format!("Failed to construct 'URL': Invalid URL: {url}")))?;
    cx.new_net_object("URL", HostData::Url(Rc::new(UrlData::new(parsed))))
}

pub(crate) fn href(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(this.borrow().as_str().to_owned())
}

pub(crate) fn set_href(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    let parsed = Url::parse(&value)
        .map_err(|_| JsThrow::Type(format!("Failed to set 'href': Invalid URL: {value}")))?;
    *this.borrow_mut() = parsed;
    Ok(())
}

pub(crate) fn origin(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::origin(&this.borrow()))
}

pub(crate) fn protocol(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::protocol(&this.borrow()))
}

pub(crate) fn set_protocol(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_protocol(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn username(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::username(&this.borrow()))
}

pub(crate) fn set_username(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_username(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn password(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::password(&this.borrow()))
}

pub(crate) fn set_password(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_password(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn host(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::host(&this.borrow()))
}

pub(crate) fn set_host(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_host(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn hostname(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::hostname(&this.borrow()))
}

pub(crate) fn set_hostname(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_hostname(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn port(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::port(&this.borrow()))
}

pub(crate) fn set_port(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_port(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn pathname(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::pathname(&this.borrow()))
}

pub(crate) fn set_pathname(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_pathname(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn search(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::search(&this.borrow()))
}

pub(crate) fn set_search(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_search(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn hash(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(parts::hash(&this.borrow()))
}

pub(crate) fn set_hash(_cx: &BindCx<'_>, this: UrlRef, value: String) -> Result<(), JsThrow> {
    parts::set_hash(&mut this.borrow_mut(), &value);
    Ok(())
}

pub(crate) fn to_json(_cx: &BindCx<'_>, this: UrlRef) -> Result<String, JsThrow> {
    Ok(this.borrow().as_str().to_owned())
}

pub(crate) fn search_params(cx: &BindCx<'_>, this: UrlRef) -> Result<JsValue, JsThrow> {
    // `[SameObject]`: return the cached wrapper (bound to this URL's live
    // query) if one exists, otherwise create and cache it.
    if let Some(cached) = this.search_params.borrow().clone() {
        return Ok(cached);
    }
    let data = Rc::new(UrlSearchParamsData::bound(Rc::clone(&this.url)));
    let wrapper = cx.new_net_object("URLSearchParams", HostData::UrlSearchParams(data))?;
    *this.search_params.borrow_mut() = Some(wrapper.clone());
    Ok(wrapper)
}
