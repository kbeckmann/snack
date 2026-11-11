//! Keep transient widget interactions within their logical owner.
use crate::Message;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay, renderer, widget::{Operation, Tree, tree}};
use iced::{Element, Event, Length, Rectangle, Size, Vector, mouse};

pub(crate) fn instance<'a, Key: Clone + PartialEq + 'static>(content: Element<'a, Message>, key: Key) -> Element<'a, Message> {
    Element::new(Instance { content, key })
}

struct Instance<'a, Key> {
    content: Element<'a, Message>,
    key: Key,
}

impl<Key: Clone + PartialEq + 'static> Widget<Message, iced::Theme, iced::Renderer> for Instance<'_, Key> {
    fn tag(&self) -> tree::Tag { tree::Tag::of::<Key>() }
    fn state(&self) -> tree::State { tree::State::new(self.key.clone()) }
    fn children(&self) -> Vec<Tree> { vec![Tree::new(&self.content)] }
    fn diff(&self, tree: &mut Tree) {
        let previous = tree.state.downcast_mut::<Key>();
        if *previous != self.key {
            *previous = self.key.clone();
            // A press, drag or IME interaction must not finish in a widget
            // that now represents a different conversation, row or control.
            tree.children = self.children();
        } else { tree.diff_children(std::slice::from_ref(&self.content)); }
    }
    fn size(&self) -> Size<Length> { self.content.as_widget().size() }
    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        self.content.as_widget_mut().layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        self.content.as_widget_mut().operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor,
        renderer: &iced::Renderer, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, viewport: &Rectangle) {
        self.content.as_widget_mut().update(&mut tree.children[0], event, layout, cursor, renderer, clipboard, shell, viewport);
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor,
        viewport: &Rectangle, renderer: &iced::Renderer) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(&tree.children[0], layout, cursor, viewport, renderer)
    }
    fn draw(&self, tree: &Tree, renderer: &mut iced::Renderer, theme: &iced::Theme,
        style: &renderer::Style, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        self.content.as_widget().draw(&tree.children[0], renderer, theme, style, layout, cursor, viewport);
    }
    fn overlay<'b>(&'b mut self, tree: &'b mut Tree, layout: Layout<'b>, renderer: &iced::Renderer,
        viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        self.content.as_widget_mut().overlay(&mut tree.children[0], layout, renderer, viewport, translation)
    }
}
