mod api;
mod pages;

use leptos::prelude::*;
use leptos_router::{
    components::{A, Route, Router, Routes},
    path,
};

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    view! {
        <Router>
            <header class="site">
                <h1>"Troop 15 Greenery Fundraiser"</h1>
                <p>"Fresh wreaths, garland and more — supporting our scouts"</p>
            </header>
            <main>
                <Routes fallback=|| view! { <p class="center">"Page not found. " <A href="/">"Back to the shop"</A></p> }>
                    <Route path=path!("/") view=pages::Shop />
                    <Route path=path!("/success") view=pages::Success />
                    <Route path=path!("/cancel") view=pages::Cancel />
                </Routes>
            </main>
        </Router>
    }
}
