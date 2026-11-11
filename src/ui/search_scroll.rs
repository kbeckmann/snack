use iced::advanced::widget::operation::{Operation, Outcome, Scrollable};
use iced::widget::Id;
use iced::{Element, Event, Length, Rectangle, Size, Vector, mouse};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay, renderer, widget::Tree};

use crate::Message;
use crate::app::MESSAGE_SCROLL_ID;

pub(crate) fn marker_id(token: uuid::Uuid) -> Id { Id::from(format!("search-layout-{token}")) }

// Scroll callbacks are omitted when content fits. Report the post-layout
// search position explicitly, including that non-scrolling case.
pub(crate) struct SearchScroll {
    token: uuid::Uuid,
    target: Option<Rectangle>,
    scroll: Option<(Rectangle, Rectangle)>,
    offset_y: Option<f32>,
    at_bottom: Option<bool>,
}

impl SearchScroll {
    pub(crate) fn new(token: uuid::Uuid) -> Self {
        Self { token, target: None, scroll: None, offset_y: None, at_bottom: None }
    }
}

impl Operation<Message> for SearchScroll {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) { operate(self); }

    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if id == Some(&marker_id(self.token)) { self.target = Some(bounds); }
    }

    fn scrollable(&mut self, id: Option<&Id>, bounds: Rectangle, content: Rectangle,
        _translation: Vector, state: &mut dyn Scrollable)
    {
        if id != Some(&Id::new(MESSAGE_SCROLL_ID)) { return; }
        self.scroll = Some((bounds, content));
        let Some(offset) = self.offset_y else { return; };
        let overflow = (content.height - bounds.height).max(0.0);
        let offset = offset.min(overflow);
        state.scroll_to(iced::widget::scrollable::AbsoluteOffset { x: Some(0.0), y: Some(offset) });
        self.at_bottom = Some(overflow == 0.0 || offset <= overflow * 0.02);
    }

    fn finish(&self) -> Outcome<Message> {
        // Measure the current result and viewport before touching scroll state.
        // A token on the result row also rejects tasks from older searches.
        if self.offset_y.is_none() {
            if let (Some(target), Some((bounds, content))) = (self.target, self.scroll) {
                let overflow = (content.height - bounds.height).max(0.0);
                let top = if target.height > bounds.height {
                    target.y - content.y
                } else {
                    target.y - content.y + (target.height - bounds.height) / 2.0
                }.clamp(0.0, overflow);
                // The transcript is anchored at the bottom, so its absolute
                // offset is measured back from the end of the content.
                return Outcome::Chain(Box::new(Self {
                    token: self.token, target: None, scroll: None,
                    offset_y: Some(overflow - top), at_bottom: None,
                }));
            }
            // The transcript may be hidden by the join panel. Finish without
            // scrolling so a pending token cannot get stuck with no task.
            return Outcome::Some(Message::SearchViewport { token: self.token, at_bottom: false });
        }
        match self.at_bottom {
            Some(at_bottom) => Outcome::Some(Message::SearchViewport { token: self.token, at_bottom }),
            None => Outcome::None,
        }
    }
}

// Keep the wrapper present in both states so toggling a pending request does
// not replace the scrollable's tree/state. Real input remains authoritative.
pub(crate) fn guard_layout_scroll(content: Element<'_, Message>, pending: bool) -> Element<'_, Message> {
    Element::new(LayoutScrollGuard { content, pending })
}

struct LayoutScrollGuard<'a> {
    content: Element<'a, Message>,
    pending: bool,
}

impl Widget<Message, iced::Theme, iced::Renderer> for LayoutScrollGuard<'_> {
    fn children(&self) -> Vec<Tree> { vec![Tree::new(&self.content)] }
    fn diff(&self, tree: &mut Tree) { tree.diff_children(std::slice::from_ref(&self.content)); }
    fn size(&self) -> Size<Length> { self.content.as_widget().size() }
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer,
        limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>,
        renderer: &iced::Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(&mut self, tree: &mut Tree, event: &Event,
        layout: Layout<'_>, cursor: mouse::Cursor, renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, viewport: &Rectangle) {
        if self.pending && matches!(event, Event::Window(iced::window::Event::RedrawRequested(_))) {
            let mut messages = Vec::new();
            let mut local = Shell::new(&mut messages);
            self.content.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, &mut local, viewport);
            shell.merge(local, |message| match message { Message::MessagesScrolled(_) => Message::Ignore, other => other });
        } else {
            self.content.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
        }
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>,
        cursor: mouse::Cursor, viewport: &Rectangle, renderer: &iced::Renderer) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn draw(&self, tree: &Tree, renderer: &mut iced::Renderer, theme: &iced::Theme,
        style: &renderer::Style, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }
    fn overlay<'b>(&'b mut self, tree: &'b mut Tree, layout: Layout<'b>,
        renderer: &iced::Renderer, viewport: &Rectangle, translation: Vector)
        -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}
