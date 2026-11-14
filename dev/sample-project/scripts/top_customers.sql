select c.email, count(*) as orders, sum(o.total) as spent
from shop.orders o
join shop.customers c on c.id = o.customer_id
group by 1
order by 3 desc
limit 20;
