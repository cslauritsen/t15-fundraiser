mod api;
mod pages;

use leptos::prelude::*;
use leptos_router::{
    components::{A, Route, Router, Routes},
    path,
};

fn main() {
    console_error_panic_hook::set_once();
    if let Some(boot) = web_sys::window().and_then(|w| w.document()).and_then(|d| d.get_element_by_id("boot")) {
        boot.remove();
    }
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let menu_open = RwSignal::new(false);

    view! {
        <Router>
            <header class="site">
                <div class="site-header">
                    <h1>"Troop 15 Greenery Fundraiser"</h1>
                    <button class="menu-toggle" type="button" aria-label="Open menu"
                        on:click=move |_| menu_open.update(|open| *open = !*open)>
                        "☰"
                    </button>
                </div>
                <p>"Fresh wreaths, garland and more — supporting our scouts"</p>
                <Show when=move || menu_open.get()>
                    <nav class="menu" aria-label="Main menu">
                        <A href="/customer-service" on:click=move |_| menu_open.set(false)>"Customer service"</A>
                    </nav>
                </Show>
            </header>
            <main>
                <Routes fallback=|| view! { <p class="center">"Page not found. " <A href="/">"Back to the shop"</A></p> }>
                    <Route path=path!("/") view=pages::Shop />
                    <Route path=path!("/customer-service") view=pages::CustomerService />
                    <Route path=path!("/success") view=pages::Success />
                    <Route path=path!("/cancel") view=pages::Cancel />
                </Routes>
            </main>
            <footer class="site">{env!("GIT_DESCRIBE")}</footer>
        </Router>
    }
}
