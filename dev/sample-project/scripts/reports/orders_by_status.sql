select status, count(*) as orders, sum(total) as total
from shop.orders
group by 1
order by 2 desc;
