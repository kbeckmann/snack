//! Reflow message metadata without replacing the body editor's widget state.
use crate::Message;
use iced::{Element, Event, Length, Rectangle, Size, Vector, mouse};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay, renderer, widget::{Operation, Tree}};

pub fn message_layout<'a>(header: Element<'a, Message>, body: Element<'a, Message>,
    badge: Option<Element<'a, Message>>, inline: bool, header_width: f32) -> Element<'a, Message>
{
    let mut children = vec![header, body];
    if let Some(badge) = badge { children.push(badge); }
    Element::new(MessageLayout { children, inline, header_width })
}

struct MessageLayout<'a> {
    children: Vec<Element<'a, Message>>,
    inline: bool,
    header_width: f32,
}

impl Widget<Message, iced::Theme, iced::Renderer> for MessageLayout<'_> {
    fn children(&self) -> Vec<Tree> { self.children.iter().map(Tree::new).collect() }
    fn diff(&self, tree: &mut Tree) { tree.diff_children(&self.children); }
    fn size(&self) -> Size<Length> { Size::new(Length::Fill, Length::Shrink) }

    fn layout(&mut self, tree: &mut Tree, renderer: &iced::Renderer, limits: &layout::Limits) -> layout::Node {
        let width = limits.max().width;
        let child_limits = |width: f32| layout::Limits::new(Size::ZERO, Size::new(width.max(0.0), limits.max().height));
        let mut header = self.children[0].as_widget_mut().layout(&mut tree.children[0], renderer,
            &child_limits(if self.inline { self.header_width.min(width) } else { width }));
        let mut badge = self.children.get_mut(2).map(|child|
            child.as_widget_mut().layout(&mut tree.children[2], renderer, &child_limits(width)));
        let (body_width, body_x, body_y, height_before_body) = if self.inline {
            let badge_width = badge.as_ref().map_or(0.0, |node| node.size().width + 4.0);
            (width - header.size().width - 4.0 - badge_width, header.size().width + 4.0, 0.0, 0.0)
        } else {
            let mut y = header.size().height + 2.0;
            if let Some(badge) = &mut badge { badge.move_to_mut((0.0, y)); y += badge.size().height + 2.0; }
            (width, 0.0, y, y)
        };
        let mut body = self.children[1].as_widget_mut().layout(&mut tree.children[1], renderer, &child_limits(body_width));
        body.move_to_mut((body_x, body_y));
        if self.inline {
            if let Some(badge) = &mut badge { badge.move_to_mut((width - badge.size().width, 0.0)); }
        }
        header.move_to_mut((0.0, 0.0));
        let height = if self.inline {
            header.size().height.max(body.size().height).max(badge.as_ref().map_or(0.0, |node| node.size().height))
        } else { height_before_body + body.size().height };
        // Child order stays fixed across layout modes, preserving focus, cursor
        // and in-progress interactions while their physical positions change.
        let mut nodes = vec![header, body];
        if let Some(badge) = badge { nodes.push(badge); }
        layout::Node::with_children(limits.resolve(Length::Fill, Length::Shrink, Size::new(width, height)), nodes)
    }

    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &iced::Renderer, operation: &mut dyn Operation) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, state), layout) in self.children.iter_mut().zip(&mut tree.children).zip(layout.children()) {
                child.as_widget_mut().operate(state, layout, renderer, operation);
            }
        });
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor,
        renderer: &iced::Renderer, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, viewport: &Rectangle) {
        for ((child, state), layout) in self.children.iter_mut().zip(&mut tree.children).zip(layout.children()) {
            child.as_widget_mut().update(state, event, layout, cursor, renderer, clipboard, shell, viewport);
        }
    }
    fn draw(&self, tree: &Tree, renderer: &mut iced::Renderer, theme: &iced::Theme, style: &renderer::Style,
        layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        if !layout.bounds().intersects(viewport) { return; }
        for ((child, state), layout) in self.children.iter().zip(&tree.children).zip(layout.children())
            .filter(|(_, layout)| layout.bounds().intersects(viewport)) {
            child.as_widget().draw(state, renderer, theme, style, layout, cursor, viewport);
        }
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle,
        renderer: &iced::Renderer) -> mouse::Interaction {
        self.children.iter().zip(&tree.children).zip(layout.children()).map(|((child, state), layout)|
            child.as_widget().mouse_interaction(state, layout, cursor, viewport, renderer)).max().unwrap_or_default()
    }
    fn overlay<'b>(&'b mut self, tree: &'b mut Tree, layout: Layout<'b>, renderer: &iced::Renderer,
        viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'b, Message, iced::Theme, iced::Renderer>> {
        overlay::from_children(&mut self.children, tree, layout, renderer, viewport, translation)
    }
}
